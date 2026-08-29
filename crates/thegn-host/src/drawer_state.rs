//! Everything drawer: the per-scope occupant cache, the keep-alive pane pool,
//! and the async cold-spawn pipeline. The built-in manager is resolved through
//! the `thegn_core::file_manager` seam, so this module is manager-agnostic — it
//! names no vendor.
//!
//! Three rules keep the drawer off the event loop's critical path:
//!
//! 1. **Flags are memory-first.** Whether a worktree's drawer is open persists
//!    as a tiny per-worktree file under `~/.thegn/drawer/` so it survives
//!    restarts, but the loop only ever reads the in-process cache ([`flag`]);
//!    writes are write-through ([`set_flag`]: cache now, file off-loop). Before
//!    this cache every tab/worktree switch paid a synchronous `read_to_string`
//!    on the loop.
//! 2. **Cold spawns resolve off-loop.** Materializing a drawer pane means
//!    resolving the manager's spawn spec + private config — so a cold
//!    [`show_drawer`] only *requests* the spec ([`request_spawn`]); a
//!    blocking task resolves it and the loop's drawer drain opens the pane when
//!    it lands (or stashes it in the pool when the user has moved on).
//! 3. **Panes are pooled.** Hiding stashes the live manager pane (position
//!    survives); showing takes it back instantly ([`DrawerPool`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use crate::compositor::Rect;
use crate::panes::Panes;
use thegn_core::config::{DrawerScope, expand_env_ref};
use thegn_core::config_drawer::{
    DrawerOccupant, DrawerPolicy, FILES_OCCUPANT_ID, GLOBAL_SCOPE_KEY, drawer_scope_key,
    resolve_drawer_cwd,
};

// ── open-flag cache ──────────────────────────────────────────────────────────

/// Memory-first drawer state keyed by scope. Values are stable occupant IDs;
/// `None` is persisted closed, while the legacy strings `true` and `false`
/// decode as `files` and closed respectively.
#[derive(Default)]
pub(crate) struct FlagCache {
    map: HashMap<String, Option<String>>,
}

impl FlagCache {
    pub(crate) fn key(dir: &Path) -> String {
        thegn_core::util::slugify(&dir.to_string_lossy())
    }
    /// Load every persisted flag from `store` — one readdir + tiny reads, only
    /// for worktrees that ever toggled a drawer.
    pub(crate) fn load_from(store: &Path) -> Self {
        let mut map = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(store) {
            for e in rd.flatten() {
                if let Ok(s) = std::fs::read_to_string(e.path()) {
                    let value = match s.trim() {
                        "true" => Some(FILES_OCCUPANT_ID.to_string()),
                        "false" | "" => None,
                        id => Some(id.to_string()),
                    };
                    map.insert(e.file_name().to_string_lossy().into_owned(), value);
                }
            }
        }
        FlagCache { map }
    }
    pub(crate) fn get(&self, dir: &Path) -> bool {
        self.occupant_for_key(&Self::key(dir)).is_some()
    }
    pub(crate) fn set(&mut self, dir: &Path, open: bool) {
        self.set_key(&Self::key(dir), open.then(|| FILES_OCCUPANT_ID.to_string()));
    }
    pub(crate) fn occupant_for_key(&self, key: &str) -> Option<String> {
        self.map.get(key).cloned().flatten()
    }
    pub(crate) fn set_key(&mut self, key: &str, occupant: Option<String>) {
        self.map.insert(key.to_string(), occupant);
    }
}

fn store_dir() -> PathBuf {
    thegn_core::util::thegn_dir().join("drawer")
}

static FLAGS: OnceLock<Mutex<FlagCache>> = OnceLock::new();

fn flags() -> &'static Mutex<FlagCache> {
    FLAGS.get_or_init(|| Mutex::new(FlagCache::load_from(&store_dir())))
}

/// Warm the flag cache from disk. Called once at startup (sanctioned pre-loop
/// I/O); after this the loop never touches the filesystem to answer "is this
/// worktree's drawer open?".
pub(crate) fn load_flags() {
    let _ = flags(); // best-effort: warm-up read: a failure just means defaults until the next flag write
}

/// Whether `dir`'s drawer is flagged open — memory only, safe on the loop.
pub(crate) fn flag(dir: &Path) -> bool {
    flags().lock().map(|f| f.get(dir)).unwrap_or(false)
}

/// Flip `dir`'s drawer flag: cache immediately, persist write-through off the
/// loop (the `persist_active_focus` pattern — a dropped write only costs the
/// flag on the next restart). The only staleness is a second concurrent thegn
/// on the same worktree, an already-accepted edge.
pub(crate) fn set_flag(dir: &Path, open: bool) {
    if let Ok(mut f) = flags().lock() {
        f.set(dir, open);
    }
    let path = store_dir().join(FlagCache::key(dir));
    let write = move || {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent); // best-effort: dir prep: a later write reports the real failure
        }
        // best-effort: the drawer flag is a UI cache; git/session are the truth.
        let _ = std::fs::write(&path, if open { "true" } else { "false" });
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(write);
    } else {
        write(); // outside the runtime (tests, teardown)
    }
}

/// Return the desired occupant for a scope from the memory cache. This is safe
/// on the event loop: startup performs the only state-directory read.
pub(crate) fn desired_occupant(scope: DrawerScope, dir: &Path) -> Option<String> {
    flags()
        .lock()
        .ok()
        .and_then(|cache| cache.occupant_for_key(&drawer_scope_key(scope, dir)))
}

/// Persist a desired occupant after updating the in-memory cache. The write is
/// deliberately off-loop and uses the same best-effort cache semantics as the
/// legacy files-drawer flag.
pub(crate) fn set_desired_occupant(scope: DrawerScope, dir: &Path, occupant: Option<&str>) {
    let key = drawer_scope_key(scope, dir);
    set_desired_key(&key, occupant);
}

fn set_desired_key(key: &str, occupant: Option<&str>) {
    if let Ok(mut cache) = flags().lock() {
        cache.set_key(key, occupant.map(str::to_string));
    }
    let path = store_dir().join(key);
    let value = occupant.unwrap_or("false").to_string();
    let write = move || {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent); // best-effort: cache directory setup
        }
        let _ = std::fs::write(&path, value); // best-effort: drawer state is recoverable UI cache
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(write);
    } else {
        write();
    }
}

fn clear_desired_if(key: &str, occupant_id: &str) {
    let matches = flags()
        .lock()
        .ok()
        .and_then(|cache| cache.occupant_for_key(key))
        .is_some_and(|current| current == occupant_id);
    if matches {
        set_desired_key(key, None);
    }
}

// ── keep-alive pane pool ─────────────────────────────────────────────────────

/// The key for a pooled drawer pane. A global occupant uses the fixed global
/// scope key, while worktree occupants use the existing slugged absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DrawerPoolKey {
    pub scope_key: String,
    pub occupant_id: String,
}

impl DrawerPoolKey {
    pub(crate) fn worktree(dir: &Path, occupant_id: impl Into<String>) -> Self {
        Self {
            scope_key: drawer_scope_key(DrawerScope::Worktree, dir),
            occupant_id: occupant_id.into(),
        }
    }

    pub(crate) fn global(occupant_id: impl Into<String>) -> Self {
        Self {
            scope_key: GLOBAL_SCOPE_KEY.to_string(),
            occupant_id: occupant_id.into(),
        }
    }
}

/// Keep-alive drawer panes, one per `(scope, occupant)` key: hiding STASHES the
/// pane (cursor position and manager state survive), showing takes it back
/// instantly, and the worktree-change detector can pre-warm the pool.
///
/// The pool is bounded by `[drawer].pool_limit`: hidden drawers are held in
/// insertion order and the oldest is evicted (its pane torn down) once the
/// limit is exceeded, so invisible manager instances cannot accumulate without
/// limit. `pool_limit = 0` disables pooling entirely (hiding kills the pane).
#[derive(Default)]
pub(crate) struct DrawerPool {
    /// `(scope/occupant key, pane-id)` in insertion order; front is oldest.
    hidden: VecDeque<(DrawerPoolKey, u32)>,
}

impl DrawerPool {
    fn legacy_key(dir: &Path) -> DrawerPoolKey {
        DrawerPoolKey::worktree(dir, FILES_OCCUPANT_ID)
    }
    /// Stash `id` for `dir`, enforcing `limit`. A `limit` of 0 tears the pane
    /// down immediately (no pool); otherwise the oldest entries beyond the
    /// limit are evicted and their panes dropped from the table.
    pub(crate) fn stash(&mut self, dir: &Path, id: u32, limit: usize, panes: &mut Panes) {
        self.stash_key(Self::legacy_key(dir), id, limit, panes);
    }

    /// Stash a pane under its complete scope/occupant key.
    pub(crate) fn stash_key(
        &mut self,
        key: DrawerPoolKey,
        id: u32,
        limit: usize,
        panes: &mut Panes,
    ) {
        if limit == 0 {
            panes.table.remove(&id);
            return;
        }
        self.remove_key(&key, panes);
        self.hidden.push_back((key, id));
        while self.hidden.len() > limit {
            if let Some((_, evicted)) = self.hidden.pop_front() {
                panes.table.remove(&evicted);
            }
        }
    }
    pub(crate) fn take(&mut self, dir: &Path) -> Option<u32> {
        self.take_key(&Self::legacy_key(dir))
    }

    pub(crate) fn take_key(&mut self, key: &DrawerPoolKey) -> Option<u32> {
        let idx = self.hidden.iter().position(|(k, _)| k == key)?;
        self.hidden.remove(idx).map(|(_, id)| id)
    }
    pub(crate) fn contains(&self, dir: &Path) -> bool {
        self.contains_key(&Self::legacy_key(dir))
    }
    pub(crate) fn contains_key(&self, key: &DrawerPoolKey) -> bool {
        self.hidden.iter().any(|(k, _)| k == key)
    }
    pub(crate) fn key_for_id(&self, id: u32) -> Option<DrawerPoolKey> {
        self.hidden
            .iter()
            .find_map(|(key, pane)| (*pane == id).then(|| key.clone()))
    }
    /// Drop a pooled entry by pane id (e.g. its manager exited on its own).
    pub(crate) fn remove_id(&mut self, id: u32) -> bool {
        let Some(idx) = self.hidden.iter().position(|(_, hid)| *hid == id) else {
            return false;
        };
        self.hidden.remove(idx);
        true
    }
    /// Drop the pooled entry for `key`, tearing down its pane.
    fn remove_key(&mut self, key: &DrawerPoolKey, panes: &mut Panes) {
        if let Some(idx) = self.hidden.iter().position(|(k, _)| k == key)
            && let Some((_, id)) = self.hidden.remove(idx)
        {
            panes.table.remove(&id);
        }
    }
}

// ── async cold spawn ─────────────────────────────────────────────────────────

/// A resolved drawer launch, produced OFF the loop by [`request_spawn`]'s
/// blocking task and consumed by the loop's drawer drain.
pub(crate) enum DrawerLaunch {
    /// The resolved file manager with its env + OOM-containment wrapper; cwd
    /// from the seam's spawn spec.
    Manager {
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
    },
    /// A configured `[[tools]]` drawer occupant. Unlike a file manager this
    /// always uses the command catalog and the local ephemeral PTY seam.
    Tool {
        argv: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    },
    /// No runnable manager (binary missing, or an empty custom command): fall
    /// back to a worktree shell pane. Rare, config-degraded; resolved
    /// synchronously at the drain (as before).
    ShellFallback,
}

/// What rides the drawer channel: the worktree the spec is for + the result.
pub(crate) type DrawerSpecMsg = (PathBuf, Result<DrawerLaunch, String>);

/// A scope-aware cold-spawn request. Both fields are required to reject a
/// result for a different occupant that raced with a picker/cycle change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrawerRequest {
    pub scope: DrawerScope,
    pub scope_key: String,
    pub occupant_id: String,
    pub worktree: PathBuf,
}

pub(crate) type DrawerRegistryMsg = (DrawerRequest, Result<DrawerLaunch, String>);

struct Spawner {
    tx: tokio_mpsc::UnboundedSender<DrawerSpecMsg>,
    waker: TerminalWaker,
    /// Worktree keys with a resolve in flight — rapid Alt+↑/↓ through a
    /// drawer-open worktree must not pile up duplicate manager panes.
    pending: Mutex<HashSet<String>>,
}

static SPAWNER: OnceLock<Spawner> = OnceLock::new();

/// Install the loop's drawer-spec channel + waker (startup, before the loop).
pub(crate) fn install_spawner(
    tx: tokio_mpsc::UnboundedSender<DrawerSpecMsg>,
    waker: TerminalWaker,
) {
    let _ = SPAWNER.set(Spawner {
        // best-effort: first-set-wins: the loop's spawner serves for the process
        tx,
        waker,
        pending: Mutex::new(HashSet::new()),
    });
}

/// Resolve `dir`'s drawer launch spec off the loop and hand it to the drawer
/// drain (channel send + waker pulse). Deduped per worktree: a second request
/// while one is in flight is a no-op. Does nothing before [`install_spawner`]
/// (headless/tests).
pub(crate) fn request_spawn(cfg: &thegn_core::config::Config, dir: &Path) {
    let Some(sp) = SPAWNER.get() else { return };
    {
        let Ok(mut pending) = sp.pending.lock() else {
            return;
        };
        if !pending.insert(FlagCache::key(dir)) {
            return;
        }
    }
    let cfg = cfg.clone();
    let dir = dir.to_path_buf();
    let tx = sp.tx.clone();
    let waker = sp.waker.clone();
    tokio::task::spawn_blocking(move || {
        // off-loop: launch_spec opens the DB and resolves the sandbox.
        let res = resolve_launch(&cfg, &dir);
        if tx.send((dir, res)).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Mark `dir`'s in-flight request consumed. The drain calls this for every
/// message it receives, so a dropped/stale spec can be re-requested later.
pub(crate) fn request_done(dir: &Path) {
    if let Some(sp) = SPAWNER.get()
        && let Ok(mut p) = sp.pending.lock()
    {
        p.remove(&FlagCache::key(dir));
    }
}

/// The off-loop half: resolve what the drawer pane should exec, through the
/// `thegn_core::file_manager` seam. The provider owns the argv/env/prepare
/// (config seeding) and the host owns the containment wrap for every kind — so
/// a custom manager is contained exactly like the default. A missing manager
/// binary (or an empty custom command) degrades to a worktree shell rather than
/// a dead pane.
fn resolve_launch(cfg: &thegn_core::config::Config, dir: &Path) -> Result<DrawerLaunch, String> {
    if !dir.is_dir() {
        return Err(format!("{}: not a directory", dir.display()));
    }
    let fm = thegn_core::file_manager::file_manager_for(cfg);
    // Seed/refresh the manager's private config (a no-op for providers without
    // config isolation). Best-effort: a failure just means the manager falls
    // back to its own defaults.
    let _ = fm.prepare();
    let spawn = fm.spawn_spec(dir);
    let Some(program) = spawn
        .argv
        .first()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Ok(DrawerLaunch::ShellFallback);
    };
    // A manager binary that does not resolve degrades to a shell — no dead pane,
    // and the doctor probe already names what is missing.
    if !manager_available(&program) {
        return Ok(DrawerLaunch::ShellFallback);
    }
    let argv = contain_drawer_argv(cfg, spawn.argv, thegn_core::util::have("systemd-run"));
    Ok(DrawerLaunch::Manager {
        argv,
        cwd: spawn.cwd.or_else(|| Some(dir.to_path_buf())),
        env: spawn.env,
    })
}

/// Whether a resolved manager `program` (`argv[0]`) is runnable: an absolute /
/// relative path that exists, or a bare name on `PATH`.
fn manager_available(program: &str) -> bool {
    if program.contains('/') {
        Path::new(program).exists()
    } else {
        thegn_core::util::which_path(program).is_some()
    }
}

struct RegistrySpawner {
    tx: tokio_mpsc::UnboundedSender<DrawerRegistryMsg>,
    waker: TerminalWaker,
    pending: Mutex<HashSet<(String, String)>>,
}

static REGISTRY_SPAWNER: OnceLock<RegistrySpawner> = OnceLock::new();

/// Install the scope-aware registry channel. Kept separate from the legacy
/// files-only channel so chunk 3 can migrate the loop in one integration edit.
pub(crate) fn install_registry_spawner(
    tx: tokio_mpsc::UnboundedSender<DrawerRegistryMsg>,
    waker: TerminalWaker,
) {
    let _ = REGISTRY_SPAWNER.set(RegistrySpawner {
        tx,
        waker,
        pending: Mutex::new(HashSet::new()),
    });
}

/// Request a configured occupant's launch spec. Config/policy expansion and
/// all PATH/filesystem checks happen in the blocking task, never on the loop.
pub(crate) fn request_occupant_spawn(
    cfg: &thegn_core::config::Config,
    scope: DrawerScope,
    occupant_id: &str,
    dir: &Path,
) {
    let Some(spawner) = REGISTRY_SPAWNER.get() else {
        return;
    };
    let scope_key = drawer_scope_key(scope, dir);
    let pending_key = (scope_key.clone(), occupant_id.to_string());
    {
        let Ok(mut pending) = spawner.pending.lock() else {
            return;
        };
        if !pending.insert(pending_key) {
            return;
        }
    }
    let request = DrawerRequest {
        scope,
        scope_key,
        occupant_id: occupant_id.to_string(),
        worktree: dir.to_path_buf(),
    };
    let tx = spawner.tx.clone();
    let waker = spawner.waker.clone();
    let cfg = cfg.clone();
    tokio::task::spawn_blocking(move || {
        let result = resolve_registry_launch(&cfg, &request);
        if tx.send((request, result)).is_ok() {
            let _ = waker.wake(); // best-effort: wake a waiting loop to drain the result
        }
    });
}

pub(crate) fn request_done_occupant(request: &DrawerRequest) {
    if let Some(spawner) = REGISTRY_SPAWNER.get()
        && let Ok(mut pending) = spawner.pending.lock()
    {
        pending.remove(&(request.scope_key.clone(), request.occupant_id.clone()));
    }
}

fn resolve_registry_launch(
    cfg: &thegn_core::config::Config,
    request: &DrawerRequest,
) -> Result<DrawerLaunch, String> {
    let policy = DrawerPolicy::from_config(cfg);
    let occupant = policy
        .occupant(&request.occupant_id)
        .ok_or_else(|| format!("unknown drawer occupant {:?}", request.occupant_id))?;
    if !occupant.available_in(request.scope) {
        return Err(format!(
            "{} is not available in {:?} scope",
            occupant.name, request.scope
        ));
    }
    if request.occupant_id == FILES_OCCUPANT_ID {
        return resolve_launch(cfg, &request.worktree);
    }
    resolve_tool_launch(occupant, request.scope, &request.worktree, cfg)
}

fn resolve_tool_launch(
    occupant: &DrawerOccupant,
    scope: DrawerScope,
    worktree: &Path,
    cfg: &thegn_core::config::Config,
) -> Result<DrawerLaunch, String> {
    let home = thegn_core::util::home();
    let cwd = resolve_drawer_cwd(scope, occupant.drawer_cwd.as_deref(), worktree, &home)?;
    if !cwd.is_dir() {
        return Err(format!("drawer cwd {} is not a directory", cwd.display()));
    }
    let argv = crate::panes::tool_drawer_argv(&occupant.command);
    if argv.is_empty() {
        return Err(format!("{} has an empty drawer command", occupant.name));
    }
    let env = occupant
        .env
        .iter()
        .filter_map(|(key, value)| expand_env_ref(value).map(|value| (key.clone(), value)))
        .collect();
    Ok(DrawerLaunch::Tool {
        argv: contain_drawer_argv(cfg, argv, thegn_core::util::have("systemd-run")),
        cwd,
        env,
    })
}

/// The loop half: openpty+exec a resolved launch — cheap and sanctioned on the
/// loop (mirrors `materialize_with_specs`' split).
pub(crate) fn open_resolved(
    panes: &mut Panes,
    launch: DrawerLaunch,
    cfg: &thegn_core::config::Config,
    dir: &Path,
    rect: Rect,
) -> Option<u32> {
    match launch {
        // The drawer is ephemeral chrome — never daemon-routed (see
        // spawn_argv_env_local).
        DrawerLaunch::Manager { argv, cwd, env } => panes
            .spawn_argv_env_local(&argv, cwd.as_deref().or(Some(dir)), &env, rect)
            .ok(),
        DrawerLaunch::Tool { argv, cwd, env } => panes
            .spawn_argv_env_local(&argv, Some(&cwd), &env, rect)
            .ok(),
        DrawerLaunch::ShellFallback => {
            crate::run::spawn_worktree_shell_pane(panes, cfg, Some(dir), rect).ok()
        }
    }
}

/// Wrap a drawer manager argv in a bounded user `systemd-run --scope` so its
/// whole process tree — including any image-preview helpers such as `ueberzugpp`,
/// which can leak to tens of GB — is OOM-killed inside its own cgroup instead of
/// triggering a global OOM that takes the terminal session down. Empty limit
/// strings omit only that property. Containment is skipped when disabled, when
/// `systemd-run` is unavailable, or when the argv already launches through
/// `systemd-run` (avoids a nested scope that would escape the bound). Applies to
/// every manager kind.
fn contain_drawer_argv(
    cfg: &thegn_core::config::Config,
    cmd: Vec<String>,
    systemd_available: bool,
) -> Vec<String> {
    if !cfg.drawer.contain
        || !systemd_available
        || cmd.first().map(String::as_str) == Some("systemd-run")
    {
        return cmd;
    }
    let mut wrapped = vec![
        "systemd-run".to_string(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
    ];
    for (key, value) in [
        ("MemoryMax", cfg.drawer.memory_max.trim()),
        ("MemorySwapMax", cfg.drawer.memory_swap_max.trim()),
        ("CPUQuota", cfg.drawer.cpu_quota.trim()),
    ] {
        if !value.is_empty() {
            wrapped.push("-p".into());
            wrapped.push(format!("{key}={value}"));
        }
    }
    wrapped.push("--".into());
    wrapped.extend(cmd);
    wrapped
}

// ── show / hide / switch-sync ────────────────────────────────────────────────

/// Show the worktree's drawer: pooled pane if alive (instant, position
/// preserved), async spec request otherwise — the pane opens when the spec
/// lands at the loop's drawer drain, so a cold spawn never blocks the switch
/// frame. Records the dir the pane belongs to in `home` so hiding stashes it
/// under the RIGHT key even after a switch.
pub(crate) fn show_drawer(
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<PathBuf>,
    cfg: &thegn_core::config::Config,
    dir: &Path,
) {
    if drawer.is_some() {
        return;
    }
    if let Some(id) = pool.take(dir) {
        *drawer = Some(id);
        *home = Some(dir.to_path_buf());
        return;
    }
    request_spawn(cfg, dir);
}

/// Hide the visible drawer, keeping its pane alive in the pool under the dir
/// it was opened for (`home`; `fallback` covers pre-tracking drawers). The
/// stash honors `[drawer].pool_limit`, evicting/tearing down older drawers.
pub(crate) fn hide_drawer_into_pool(
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<PathBuf>,
    fallback: &Path,
    cfg: &thegn_core::config::Config,
    panes: &mut Panes,
) {
    if let Some(id) = drawer.take() {
        let key = home.take().unwrap_or_else(|| fallback.to_path_buf());
        pool.stash(&key, id, cfg.drawer.pool_limit, panes);
    }
}

/// Reconcile the visible drawer with the active worktree's persisted flag on a
/// tab/worktree switch: stash a drawer belonging to another worktree, then show
/// (pool-or-request) / hide per the flag. Reads only the in-memory [`flag`]
/// cache — no filesystem on the loop. `_center` is kept so the many existing
/// call sites don't churn; a cold spawn is sized at the drawer drain instead.
pub(crate) fn sync_drawer_persistence(
    session: &crate::session::Session,
    panes: &mut Panes,
    drawer: &mut Option<u32>,
    pool: &mut DrawerPool,
    home: &mut Option<PathBuf>,
    cfg: &thegn_core::config::Config,
    _center: Rect,
) {
    let _g = crate::perf::measure(crate::perf::Subsys::Drawer);
    let Some(dir) = crate::run::active_cwd(session) else {
        return;
    };
    let should_be_open = flag(&dir);

    // The visible drawer belongs to whichever worktree opened it; on a
    // mismatch stash it under ITS home before deciding for the new one.
    if drawer.is_some() && home.as_deref() != Some(dir.as_path()) {
        hide_drawer_into_pool(drawer, pool, home, &dir, cfg, panes);
    }
    if should_be_open && drawer.is_none() {
        show_drawer(drawer, pool, home, cfg, &dir);
    } else if !should_be_open && drawer.is_some() {
        hide_drawer_into_pool(drawer, pool, home, &dir, cfg, panes);
    }
}

/// The currently visible drawer occupant. The pool key is authoritative for
/// reuse; `home` is retained for the legacy file-manager compatibility path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleDrawer {
    pub key: DrawerPoolKey,
    pub scope: DrawerScope,
    pub worktree: PathBuf,
    pub pane_id: u32,
}

/// Scope-aware drawer lifecycle shared by keyboard actions and the loop drain.
/// It contains only in-memory state and delegates cold work to the channel
/// spawner above.
#[derive(Default)]
pub(crate) struct DrawerRuntime {
    pub visible: Option<VisibleDrawer>,
    pub pool: DrawerPool,
    last: HashMap<String, String>,
}

impl DrawerRuntime {
    fn target(dir: &Path) -> Option<(DrawerScope, DrawerPoolKey, String)> {
        if let Some(id) = desired_occupant(DrawerScope::Worktree, dir) {
            return Some((DrawerScope::Worktree, DrawerPoolKey::worktree(dir, &id), id));
        }
        desired_occupant(DrawerScope::Global, dir)
            .map(|id| (DrawerScope::Global, DrawerPoolKey::global(&id), id))
    }

    fn stash_visible(&mut self, cfg: &thegn_core::config::Config, panes: &mut Panes) {
        if let Some(visible) = self.visible.take() {
            self.pool
                .stash_key(visible.key, visible.pane_id, cfg.drawer.pool_limit, panes);
        }
    }

    fn show_target(
        &mut self,
        cfg: &thegn_core::config::Config,
        scope: DrawerScope,
        key: DrawerPoolKey,
        occupant_id: String,
        dir: &Path,
    ) {
        if let Some(pane_id) = self.pool.take_key(&key) {
            self.visible = Some(VisibleDrawer {
                key,
                scope,
                worktree: dir.to_path_buf(),
                pane_id,
            });
        } else {
            request_occupant_spawn(cfg, scope, &occupant_id, dir);
        }
    }

    /// Reconcile visibility after a worktree switch. An open worktree slot
    /// wins over an open global slot; the outgoing process is stashed by its
    /// complete scope/occupant key.
    pub(crate) fn reconcile(
        &mut self,
        cfg: &thegn_core::config::Config,
        dir: &Path,
        panes: &mut Panes,
        _rect: Rect,
    ) {
        let wanted = Self::target(dir);
        if let Some((scope, key, id)) = wanted {
            if self
                .visible
                .as_ref()
                .is_some_and(|visible| visible.key == key)
            {
                return;
            }
            if self.visible.is_some() {
                self.stash_visible(cfg, panes);
            }
            self.show_target(cfg, scope, key, id, dir);
        } else if self.visible.is_some() {
            self.stash_visible(cfg, panes);
        }
    }

    /// Select an occupant for a scope. Global selection of the built-in files
    /// occupant is normalized to the active worktree files slot because the
    /// global state slot is reserved for configured global tools.
    pub(crate) fn select(
        &mut self,
        cfg: &thegn_core::config::Config,
        scope: DrawerScope,
        occupant_id: &str,
        dir: &Path,
        panes: &mut Panes,
        rect: Rect,
    ) -> bool {
        let policy = DrawerPolicy::from_config(cfg);
        let Some(occupant) = policy.occupant(occupant_id) else {
            return false;
        };
        if !occupant.available_in(scope) {
            return false;
        }
        let (state_scope, id) = if scope == DrawerScope::Global && occupant_id == FILES_OCCUPANT_ID
        {
            (DrawerScope::Worktree, FILES_OCCUPANT_ID)
        } else {
            (scope, occupant_id)
        };
        self.last
            .insert(drawer_scope_key(state_scope, dir), id.to_string());
        set_desired_occupant(state_scope, dir, Some(id));
        self.reconcile(cfg, dir, panes, rect);
        true
    }

    /// Close the active scope's desired occupant. The live pane is reconciled
    /// immediately, while any pooled pane remains available only until its
    /// persisted slot is selected again.
    pub(crate) fn close(
        &mut self,
        cfg: &thegn_core::config::Config,
        scope: DrawerScope,
        dir: &Path,
        panes: &mut Panes,
        rect: Rect,
    ) {
        set_desired_occupant(scope, dir, None);
        self.reconcile(cfg, dir, panes, rect);
    }

    /// Toggle the last occupant in a scope. Closing remembers the occupant so
    /// reopening the compact files action restores the same tool; a fresh
    /// scope falls back to the built-in files occupant.
    pub(crate) fn toggle(
        &mut self,
        cfg: &thegn_core::config::Config,
        scope: DrawerScope,
        dir: &Path,
        panes: &mut Panes,
        rect: Rect,
    ) {
        if desired_occupant(scope, dir).is_some() {
            self.close(cfg, scope, dir, panes, rect);
            return;
        }
        let key = drawer_scope_key(scope, dir);
        let id = self
            .last
            .get(&key)
            .cloned()
            .unwrap_or_else(|| FILES_OCCUPANT_ID.into());
        if scope == DrawerScope::Global && id == FILES_OCCUPANT_ID {
            return;
        }
        let _ = self.select(cfg, scope, &id, dir, panes, rect);
    }

    /// Cycle through the effective registry in config order. The files row is
    /// always first and remains the fallback when no configured occupant is
    /// available for the requested scope.
    pub(crate) fn cycle(
        &mut self,
        cfg: &thegn_core::config::Config,
        scope: DrawerScope,
        dir: &Path,
        panes: &mut Panes,
        rect: Rect,
    ) -> Option<String> {
        let policy = DrawerPolicy::from_config(cfg);
        let occupants = policy.occupants_for(scope);
        if occupants.is_empty() {
            return None;
        }
        let current = desired_occupant(scope, dir).unwrap_or_else(|| FILES_OCCUPANT_ID.into());
        let index = occupants
            .iter()
            .position(|occupant| occupant.id == current)
            .unwrap_or(0);
        let next = occupants[(index + 1) % occupants.len()].id.clone();
        if scope == DrawerScope::Global && next == FILES_OCCUPANT_ID {
            self.close(cfg, scope, dir, panes, rect);
        } else {
            self.select(cfg, scope, &next, dir, panes, rect);
        }
        Some(next)
    }

    /// Apply one cold result. A result is opened only when both scope key and
    /// occupant still match the desired state; otherwise it is discarded.
    pub(crate) fn apply_result(
        &mut self,
        cfg: &thegn_core::config::Config,
        request: DrawerRequest,
        result: Result<DrawerLaunch, String>,
        dir: &Path,
        panes: &mut Panes,
        rect: Rect,
    ) {
        request_done_occupant(&request);
        let desired = desired_occupant(request.scope, &request.worktree);
        if desired.as_deref() != Some(request.occupant_id.as_str()) {
            return;
        }
        let key = DrawerPoolKey {
            scope_key: request.scope_key.clone(),
            occupant_id: request.occupant_id.clone(),
        };
        let launch = match result {
            Ok(launch) => launch,
            Err(error) => {
                thegn_core::msg::warn(&format!("drawer {}: {error}", request.occupant_id));
                set_desired_key(&request.scope_key, None);
                if self
                    .visible
                    .as_ref()
                    .is_some_and(|visible| visible.key == key)
                {
                    self.visible = None;
                }
                return;
            }
        };
        let active = Self::target(dir).is_some_and(|(_, active_key, _)| active_key == key);
        if active && self.visible.is_none() {
            if let Some(pane_id) = open_resolved(panes, launch, cfg, &request.worktree, rect) {
                self.visible = Some(VisibleDrawer {
                    key,
                    scope: request.scope,
                    worktree: request.worktree,
                    pane_id,
                });
            } else {
                thegn_core::msg::warn(&format!("drawer {} failed to spawn", request.occupant_id));
                set_desired_key(&request.scope_key, None);
            }
        } else if cfg.drawer.pool_limit > 0
            && !self.pool.contains_key(&key)
            && let Some(pane_id) = open_resolved(panes, launch, cfg, &request.worktree, rect)
        {
            self.pool
                .stash_key(key, pane_id, cfg.drawer.pool_limit, panes);
        }
    }

    /// Remove a drawer pane from the visible slot or pool and clear its
    /// persisted desired occupant. This is called when the PTY exits on its
    /// own, so a dead process cannot be resurrected on the next switch.
    pub(crate) fn on_exit(&mut self, pane_id: u32, panes: &mut Panes) {
        if let Some(key) = self.pool.key_for_id(pane_id) {
            self.pool.remove_id(pane_id);
            clear_desired_if(&key.scope_key, &key.occupant_id);
            panes.table.remove(&pane_id);
        }
        if self
            .visible
            .as_ref()
            .is_some_and(|visible| visible.pane_id == pane_id)
        {
            if let Some(visible) = self.visible.take() {
                clear_desired_if(&visible.key.scope_key, &visible.key.occupant_id);
            }
            panes.table.remove(&pane_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PANE_EVENT_CHANNEL_CAPACITY: usize = 256;

    #[test]
    fn flag_cache_round_trips_and_defaults_closed() {
        let mut c = FlagCache::default();
        let a = Path::new("/tmp/wt-a");
        let b = Path::new("/tmp/wt-b");
        assert!(!c.get(a), "unknown dirs default to closed");
        c.set(a, true);
        assert!(c.get(a));
        assert!(!c.get(b), "flags are per-worktree");
        c.set(a, false);
        assert!(!c.get(a));
    }

    #[test]
    fn flag_cache_loads_persisted_files() {
        let store = std::env::temp_dir().join(format!("tg-drawer-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&store).unwrap();
        let open = Path::new("/tmp/wt-open");
        let closed = Path::new("/tmp/wt-closed");
        std::fs::write(store.join(FlagCache::key(open)), "true\n").unwrap();
        std::fs::write(store.join(FlagCache::key(closed)), "false").unwrap();

        let c = FlagCache::load_from(&store);
        assert!(c.get(open), "whitespace-tolerant true");
        assert!(!c.get(closed));
        assert!(!c.get(Path::new("/tmp/wt-never")), "missing file = closed");

        let empty = FlagCache::load_from(&store.join("nope"));
        assert!(!empty.get(open), "missing store dir = all closed");
        let _ = std::fs::remove_dir_all(&store); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn state_cache_decodes_legacy_flags_and_occupant_ids() {
        let store =
            std::env::temp_dir().join(format!("tg-drawer-occupants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store); // best-effort: test scratch cleanup
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("legacy-open"), "true").unwrap();
        std::fs::write(store.join("legacy-closed"), "false").unwrap();
        std::fs::write(store.join("tool"), "tool:db\n").unwrap();

        let cache = FlagCache::load_from(&store);
        assert_eq!(cache.occupant_for_key("legacy-open"), Some("files".into()));
        assert_eq!(cache.occupant_for_key("legacy-closed"), None);
        assert_eq!(cache.occupant_for_key("tool"), Some("tool:db".into()));
        let _ = std::fs::remove_dir_all(&store); // best-effort: test scratch cleanup
    }

    #[test]
    fn pool_keys_isolate_occupants_and_global_scope() {
        let worktree = Path::new("/tmp/wt");
        let files = DrawerPoolKey::worktree(worktree, FILES_OCCUPANT_ID);
        let tool = DrawerPoolKey::worktree(worktree, "tool:db");
        let global = DrawerPoolKey::global("tool:db");
        assert_ne!(files, tool);
        assert_ne!(tool, global);

        let (tx, _rx) = tokio_mpsc::channel(TEST_PANE_EVENT_CHANNEL_CAPACITY);
        let mut panes = Panes::new(tx);
        let mut pool = DrawerPool::default();
        pool.stash_key(tool.clone(), 7, 3, &mut panes);
        assert!(pool.contains_key(&tool));
        assert!(!pool.contains_key(&global));
        assert_eq!(pool.take_key(&tool), Some(7));
    }

    #[test]
    fn contain_drawer_argv_wraps_scope_with_drawer_limits() {
        let cfg = thegn_core::config::Config::default();
        let argv = contain_drawer_argv(&cfg, vec!["fm".into()], true);

        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--user".to_string()));
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.contains(&"--collect".to_string()));
        assert!(argv.contains(&"MemoryMax=2G".to_string()));
        assert!(argv.contains(&"MemorySwapMax=512M".to_string()));
        assert!(argv.contains(&"CPUQuota=200%".to_string()));
        // The wrapped command follows the `--` separator.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], &["fm".to_string()]);
    }

    #[test]
    fn contain_drawer_argv_omits_empty_limits_and_can_disable() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.drawer.memory_swap_max.clear();
        cfg.drawer.cpu_quota.clear();
        let argv = contain_drawer_argv(&cfg, vec!["fm".into()], true);
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"MemoryMax=2G".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("MemorySwapMax=")));
        assert!(!argv.iter().any(|a| a.starts_with("CPUQuota=")));

        // Disabled, missing systemd-run, or an already-wrapped sandbox argv all
        // pass the command through untouched.
        cfg.drawer.contain = false;
        assert_eq!(
            contain_drawer_argv(&cfg, vec!["fm".into()], true),
            vec!["fm"]
        );
        cfg.drawer.contain = true;
        assert_eq!(
            contain_drawer_argv(&cfg, vec!["fm".into()], false),
            vec!["fm"]
        );
        let nested = vec!["systemd-run".to_string(), "--user".into(), "--pty".into()];
        assert_eq!(contain_drawer_argv(&cfg, nested.clone(), true), nested);
    }

    /// Vendor-isolation guard (`file-explorer` spec): the generic drawer module
    /// names no file-manager vendor — every vendor specific lives behind the
    /// `thegn_core::file_manager` seam. The search term is assembled at runtime
    /// so this test source stays vendor-free too (it `include_str!`s itself).
    #[test]
    fn generic_drawer_code_names_no_vendor_symbol() {
        let src = include_str!("drawer_state.rs");
        let vendor = concat!("ya", "zi");
        let n = src.to_ascii_lowercase().matches(vendor).count();
        assert_eq!(
            n, 0,
            "drawer_state.rs must not name the `{vendor}` vendor — move the \
             specific behind the file_manager seam"
        );
    }
}
