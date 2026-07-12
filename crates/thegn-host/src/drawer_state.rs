//! Everything drawer: the per-worktree open-flag cache, the keep-alive yazi
//! pane pool, and the async cold-spawn pipeline.
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
//!    `agent::launch_spec` — DB opens + sandbox resolution — so a cold
//!    [`show_yazi_drawer`] only *requests* the spec ([`request_spawn`]); a
//!    blocking task resolves it and the loop's drawer drain opens the pane when
//!    it lands (or stashes it in the pool when the user has moved on).
//! 3. **Panes are pooled.** Hiding stashes the live yazi (position survives);
//!    showing takes it back instantly ([`DrawerPool`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use crate::compositor::Rect;
use crate::panes::Panes;

// ── open-flag cache ──────────────────────────────────────────────────────────

/// Pure flag cache keyed by worktree slug. The global wrappers below bind it
/// to the on-disk store; kept separate so the semantics are unit-testable
/// against an explicit directory (no process-env mutation in tests).
#[derive(Default)]
pub(crate) struct FlagCache {
    map: HashMap<String, bool>,
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
                    map.insert(
                        e.file_name().to_string_lossy().into_owned(),
                        s.trim() == "true",
                    );
                }
            }
        }
        FlagCache { map }
    }
    pub(crate) fn get(&self, dir: &Path) -> bool {
        self.map.get(&Self::key(dir)).copied().unwrap_or(false)
    }
    pub(crate) fn set(&mut self, dir: &Path, open: bool) {
        self.map.insert(Self::key(dir), open);
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
    let _ = flags();
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
            let _ = std::fs::create_dir_all(parent);
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

// ── keep-alive pane pool ─────────────────────────────────────────────────────

/// Keep-alive yazi drawers, one per worktree dir: hiding STASHES the pane
/// (cursor position and yazi state survive), showing takes it back
/// instantly, and the worktree-change detector pre-warms the pool so the
/// first toggle never waits on yazi's startup.
///
/// The pool is bounded by `[drawer].pool_limit`: hidden drawers are held in
/// insertion order and the oldest is evicted (its pane torn down) once the
/// limit is exceeded, so invisible yazi instances cannot accumulate without
/// limit. `pool_limit = 0` disables pooling entirely (hiding kills the pane).
#[derive(Default)]
pub(crate) struct DrawerPool {
    /// `(dir-key, pane-id)` in insertion order; front is the oldest (next to evict).
    hidden: VecDeque<(String, u32)>,
}

impl DrawerPool {
    fn key(dir: &Path) -> String {
        FlagCache::key(dir)
    }
    /// Stash `id` for `dir`, enforcing `limit`. A `limit` of 0 tears the pane
    /// down immediately (no pool); otherwise the oldest entries beyond the
    /// limit are evicted and their panes dropped from the table.
    pub(crate) fn stash(&mut self, dir: &Path, id: u32, limit: usize, panes: &mut Panes) {
        if limit == 0 {
            panes.table.remove(&id);
            return;
        }
        let key = Self::key(dir);
        self.remove_key(&key, panes);
        self.hidden.push_back((key, id));
        while self.hidden.len() > limit {
            if let Some((_, evicted)) = self.hidden.pop_front() {
                panes.table.remove(&evicted);
            }
        }
    }
    pub(crate) fn take(&mut self, dir: &Path) -> Option<u32> {
        let key = Self::key(dir);
        let idx = self.hidden.iter().position(|(k, _)| k == &key)?;
        self.hidden.remove(idx).map(|(_, id)| id)
    }
    pub(crate) fn contains(&self, dir: &Path) -> bool {
        let key = Self::key(dir);
        self.hidden.iter().any(|(k, _)| k == &key)
    }
    /// Drop a pooled entry by pane id (e.g. its yazi exited on its own).
    pub(crate) fn remove_id(&mut self, id: u32) -> bool {
        let Some(idx) = self.hidden.iter().position(|(_, hid)| *hid == id) else {
            return false;
        };
        self.hidden.remove(idx);
        true
    }
    /// Drop the pooled entry for `key`, tearing down its pane.
    fn remove_key(&mut self, key: &str, panes: &mut Panes) {
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
    /// yazi with its env + OOM-containment wrapper; cwd from the launch spec.
    Yazi {
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
    },
    /// yazi isn't installed: fall back to a worktree shell pane. Rare,
    /// config-degraded; resolved synchronously at the drain (as before).
    ShellFallback,
}

/// What rides the drawer channel: the worktree the spec is for + the result.
pub(crate) type DrawerSpecMsg = (PathBuf, Result<DrawerLaunch, String>);

struct Spawner {
    tx: tokio_mpsc::UnboundedSender<DrawerSpecMsg>,
    waker: TerminalWaker,
    /// Worktree keys with a resolve in flight — rapid Alt+↑/↓ through a
    /// drawer-open worktree must not pile up duplicate yazis.
    pending: Mutex<HashSet<String>>,
}

static SPAWNER: OnceLock<Spawner> = OnceLock::new();

/// Install the loop's drawer-spec channel + waker (startup, before the loop).
pub(crate) fn install_spawner(
    tx: tokio_mpsc::UnboundedSender<DrawerSpecMsg>,
    waker: TerminalWaker,
) {
    let _ = SPAWNER.set(Spawner {
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
            let _ = waker.wake();
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

/// The off-loop half: resolve what the drawer pane should exec.
fn resolve_launch(cfg: &thegn_core::config::Config, dir: &Path) -> Result<DrawerLaunch, String> {
    if !dir.is_dir() {
        return Err(format!("{}: not a directory", dir.display()));
    }
    if cfg.tool_command("yazi").is_none() {
        return Ok(DrawerLaunch::ShellFallback);
    }
    let wt = dir.to_string_lossy().into_owned();
    let spec = crate::agent::launch_spec(cfg, &wt, None, "yazi").map_err(|e| e.to_string())?;
    let argv = contain_yazi_argv(cfg, spec.argv, thegn_core::util::have("systemd-run"));
    Ok(DrawerLaunch::Yazi {
        argv,
        cwd: spec.cwd,
        env: crate::panes::yazi_env(cfg),
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
        DrawerLaunch::Yazi { argv, cwd, env } => panes
            .spawn_argv_env_local(&argv, cwd.as_deref().or(Some(dir)), &env, rect)
            .ok(),
        DrawerLaunch::ShellFallback => {
            crate::run::spawn_worktree_shell_pane(panes, cfg, Some(dir), rect, false, None, "").ok()
        }
    }
}

/// Wrap a drawer yazi argv in a bounded user `systemd-run --scope` so its whole
/// process tree — including image-preview helpers such as `ueberzugpp`, which
/// can leak to tens of GB — is OOM-killed inside its own cgroup instead of
/// triggering a global OOM that takes the terminal session down. Empty limit
/// strings omit only that property. Containment is skipped when disabled, when
/// `systemd-run` is unavailable, or when the resolved sandbox already launches
/// through `systemd-run` (avoids a nested scope that would escape the bound).
fn contain_yazi_argv(
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
pub(crate) fn show_yazi_drawer(
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
        show_yazi_drawer(drawer, pool, home, cfg, &dir);
    } else if !should_be_open && drawer.is_some() {
        hide_drawer_into_pool(drawer, pool, home, &dir, cfg, panes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let store = std::env::temp_dir().join(format!("sz-drawer-flags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
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
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn contain_yazi_argv_wraps_scope_with_drawer_limits() {
        let cfg = thegn_core::config::Config::default();
        let argv = contain_yazi_argv(&cfg, vec!["yazi".into()], true);

        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--user".to_string()));
        assert!(argv.contains(&"--scope".to_string()));
        assert!(argv.contains(&"--collect".to_string()));
        assert!(argv.contains(&"MemoryMax=2G".to_string()));
        assert!(argv.contains(&"MemorySwapMax=512M".to_string()));
        assert!(argv.contains(&"CPUQuota=200%".to_string()));
        // The wrapped command follows the `--` separator.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], &["yazi".to_string()]);
    }

    #[test]
    fn contain_yazi_argv_omits_empty_limits_and_can_disable() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.drawer.memory_swap_max.clear();
        cfg.drawer.cpu_quota.clear();
        let argv = contain_yazi_argv(&cfg, vec!["yazi".into()], true);
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"MemoryMax=2G".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("MemorySwapMax=")));
        assert!(!argv.iter().any(|a| a.starts_with("CPUQuota=")));

        // Disabled, missing systemd-run, or an already-wrapped sandbox argv all
        // pass the command through untouched.
        cfg.drawer.contain = false;
        assert_eq!(
            contain_yazi_argv(&cfg, vec!["yazi".into()], true),
            vec!["yazi"]
        );
        cfg.drawer.contain = true;
        assert_eq!(
            contain_yazi_argv(&cfg, vec!["yazi".into()], false),
            vec!["yazi"]
        );
        let nested = vec!["systemd-run".to_string(), "--user".into(), "--pty".into()];
        assert_eq!(contain_yazi_argv(&cfg, nested.clone(), true), nested);
    }
}
