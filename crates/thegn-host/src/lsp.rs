//! Host-side LSP integration: the [`LspSupervisor`] (lazy, warm, per-worktree
//! server lifecycle) and the [`LspDiagnostics`] store that folds server-pushed
//! diagnostics into the existing Problems panel.
//!
//! The supervisor owns the [`thegn_svc::lsp::LspClient`] connections, keyed
//! by `(worktree_root, language)`. Servers are **never** started eagerly — the
//! first request for a `(root, lang)` spawns and initializes one, and it then
//! stays warm across tab switches (rust-analyzer's index is expensive). The
//! inner state is `Arc`-shared so an off-loop request task can lazily start and
//! reuse clients without blocking the render loop.
//!
//! Diagnostics arrive asynchronously on each client's reader thread and land
//! in the bounded [`thegn_svc::lsp::DiagnosticsSender`] bus. The host runs a
//! bridge thread (it owns the `TerminalWaker`; svc does not) that blocks on the
//! bus's single wake obligation and pulses the waker; the loop then drains a
//! bounded slice per iteration with [`drain_diagnostics`].
//!
//! Lifecycle: every client is registered on the bus under a supervisor-minted
//! generation. [`LspInner::reconcile_roots`] (driven off-loop from the host's
//! worktree set) retires closed worktrees' streams, frees their registry and
//! negative-cache slots, and tears the subprocesses down on a worker thread;
//! the store then drops exactly the retired streams' data, proven by the bus's
//! registration set rather than by any number carried on a publication.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thegn_core::config::Config;
use thegn_svc::lsp::{
    DiagnosticKey, DiagnosticsReceiver, DiagnosticsSender, LspClient, LspDiagnostic, LspError,
    LspHealth, LspSeverity, PublishedDiagnostics, Registry, StreamKey, limits,
};

use crate::panel::{DiagnosticItem, Severity};

/// Shared LSP state, owned by the event loop and cloned into off-loop tasks.
pub struct LspSupervisor {
    inner: Arc<LspInner>,
    /// Receiver end of the diagnostics bus handed to clients; taken once by
    /// the host to drive the bridge thread and the budgeted drain.
    raw_rx: Option<DiagnosticsReceiver>,
}

/// One `(root, registry key)` slot: a started client under its registered
/// generation, or a cached "no server" (so we don't re-spawn on every
/// request). Both count toward the root / servers-per-root bounds and both are
/// released by [`LspInner::reconcile_roots`] / [`LspInner::close`].
enum ClientSlot {
    Live {
        client: Arc<LspClient>,
        generation: u64,
    },
    Unavailable,
}

/// Keying on the registry key (not the tree-sitter `Lang`) is what lets an
/// arbitrary language server — `zls`, `clangd`, an in-house DSL server — hold
/// a per-worktree instance.
type ClientMap = HashMap<(PathBuf, String), ClientSlot>;

pub struct LspInner {
    enabled: bool,
    /// The resolved server registry (built-ins + user `[[lsp.servers]]`). Built
    /// once at startup and immutable, so it needs no lock and is shared read-only
    /// across every off-loop request task.
    registry: Registry,
    diag_tx: DiagnosticsSender,
    clients: Mutex<ClientMap>,
}

impl LspSupervisor {
    /// Build from config. Starts nothing; just records the policy + registry.
    pub fn from_config(cfg: &Config) -> Self {
        let (diag_tx, raw_rx) = thegn_svc::lsp::diagnostics_channel();
        LspSupervisor {
            inner: Arc::new(LspInner {
                enabled: cfg.lsp.enabled,
                registry: Registry::build(&cfg.lsp.servers),
                diag_tx,
                clients: Mutex::new(HashMap::new()),
            }),
            raw_rx: Some(raw_rx),
        }
    }

    /// Take the diagnostics receiver to drive the host bridge thread (once).
    pub fn take_diagnostics_rx(&mut self) -> Option<DiagnosticsReceiver> {
        self.raw_rx.take()
    }

    /// An `Arc` handle for use inside an off-loop (`spawn_blocking`) task.
    pub fn handle(&self) -> Arc<LspInner> {
        self.inner.clone()
    }
}

/// Drop clients on a worker thread: `LspClient::drop` kills and reaps a child,
/// which must never run on the render/input thread.
fn teardown_off_loop(clients: Vec<Arc<LspClient>>) {
    if clients.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("thegn-lsp-teardown".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            drop(clients);
        });
    if let Err(error) = spawned {
        tracing::warn!(target: "thegn::lsp", %error, "LSP teardown thread failed to spawn");
    }
}

impl LspInner {
    /// Resolve a file path to its registry key (the LSP tier), by extension.
    /// Pure; safe to call anywhere. `None` for an unregistered extension.
    pub fn resolve_key(&self, path: &str) -> Option<String> {
        self.registry.resolve_key(path)
    }

    /// Get the warm client for `(root, key)`, lazily spawning+initializing it on
    /// first use. **Blocks** (spawn + initialize) — call off the event loop.
    pub fn client(&self, root: &Path, key: &str) -> Result<Arc<LspClient>, LspError> {
        self.client_with(root, key, |root, key, diagnostics, generation| {
            let Some(spec) = self.registry.resolve(key) else {
                return Err(LspError::NotAvailable);
            };
            // Join the shared aggregate slice, like every pane and background
            // job. rust-analyzer alone can take gigabytes — a language server
            // is exactly the background hog the slice exists to bound. The
            // wrap is fail-safe: no published policy / unusable systemd-run ⇒
            // the server spawns unwrapped, exactly as before. Off-loop, so the
            // wrap's probe spawn is fine here.
            let argv = thegn_core::sandbox_cpucap::wrap_background_argv(spec.argv());
            let client = LspClient::start_argv_with_identity(
                &argv,
                &spec.language_id,
                root,
                diagnostics,
                key.to_string(),
                generation,
            )?;
            client.initialize(root)?;
            Ok(Arc::new(client))
        })
    }

    /// [`LspInner::client`] with an injectable starter (tests drive the
    /// lifecycle with in-memory transports). The starter receives the bus and
    /// the freshly registered generation it must publish under.
    pub(crate) fn client_with(
        &self,
        root: &Path,
        key: &str,
        start: impl FnOnce(&Path, &str, DiagnosticsSender, u64) -> Result<Arc<LspClient>, LspError>,
    ) -> Result<Arc<LspClient>, LspError> {
        if !self.enabled {
            return Err(LspError::NotAvailable);
        }
        // Bounds on the borrowed inputs, before anything is cloned.
        if root.as_os_str().is_empty()
            || root.as_os_str().as_encoded_bytes().len() > limits::MAX_IDENTITY_BYTES
            || key.is_empty()
            || key.len() > limits::MAX_IDENTITY_BYTES
        {
            return Err(LspError::Bounded("LSP authority identity limit".into()));
        }
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| LspError::Protocol("LSP client map poisoned".into()))?;
        let map_key = (root.to_path_buf(), key.to_string());
        if let Some(slot) = clients.get(&map_key) {
            return match slot {
                ClientSlot::Live { client, .. } => Ok(client.clone()),
                ClientSlot::Unavailable => Err(LspError::NotAvailable),
            };
        }
        let root_known = clients.keys().any(|(existing, _)| existing == root);
        if !root_known
            && clients
                .keys()
                .map(|(existing, _)| existing)
                .collect::<HashSet<_>>()
                .len()
                >= limits::MAX_ROOTS
        {
            return Err(LspError::Bounded("LSP root limit reached".into()));
        }
        if clients
            .keys()
            .filter(|(existing, _)| existing == root)
            .count()
            >= limits::MAX_SERVERS_PER_ROOT
        {
            return Err(LspError::Bounded(
                "LSP servers-per-root limit reached".into(),
            ));
        }
        let generation = self.diag_tx.register(root, key)?;
        match start(root, key, self.diag_tx.clone(), generation) {
            Ok(client) => {
                clients.insert(
                    map_key,
                    ClientSlot::Live {
                        client: client.clone(),
                        generation,
                    },
                );
                Ok(client)
            }
            Err(e) => {
                self.diag_tx.retire(root, key, generation);
                // Cache "no server" so we don't try to spawn on every request.
                if e == LspError::NotAvailable {
                    clients.insert(map_key, ClientSlot::Unavailable);
                }
                Err(e)
            }
        }
    }

    /// Retire `(root, key)`'s stream and tear its client down off-loop. A late
    /// publication from the old reader is stale from this point on.
    #[allow(dead_code)] // single-server close seam; the loop reconciles whole roots
    pub fn close(&self, root: &Path, key: &str) {
        let removed = self
            .clients
            .lock()
            .ok()
            .and_then(|mut clients| clients.remove(&(root.to_path_buf(), key.to_string())));
        if let Some(ClientSlot::Live { client, generation }) = removed {
            self.diag_tx.retire(root, key, generation);
            teardown_off_loop(vec![client]);
        }
    }

    /// Retire every server (and negative-cache slot) whose worktree is no
    /// longer in `live_roots`, releasing registry capacity; subprocess
    /// teardown happens on a worker thread. Blocks on the client map (which a
    /// concurrent spawn may hold) — call off the event loop. Returns how many
    /// slots were released.
    pub fn reconcile_roots(&self, live_roots: &HashSet<PathBuf>) -> usize {
        let mut teardown = Vec::new();
        let mut released = 0usize;
        if let Ok(mut clients) = self.clients.lock() {
            let stale: Vec<(PathBuf, String)> = clients
                .keys()
                .filter(|(root, _)| !live_roots.contains(root))
                .cloned()
                .collect();
            for map_key in stale {
                let Some(slot) = clients.remove(&map_key) else {
                    continue;
                };
                released += 1;
                if let ClientSlot::Live { client, generation } = slot {
                    self.diag_tx.retire(&map_key.0, &map_key.1, generation);
                    teardown.push(client);
                }
            }
        }
        teardown_off_loop(teardown);
        released
    }
}

/// Shuts the diagnostics bus down when dropped: frees queued publications,
/// loss marks and registrations and lets the bridge thread exit, on every
/// path out of the event loop.
pub struct BusShutdown(DiagnosticsReceiver);

impl BusShutdown {
    pub fn new(rx: DiagnosticsReceiver) -> Self {
        BusShutdown(rx)
    }
}

impl Drop for BusShutdown {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

// ─── the budgeted drain ──────────────────────────────────────────────────────

/// Per-iteration host budget for applying diagnostics (see `limits::DRAIN_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainBudget {
    pub publications: usize,
    pub bytes: usize,
    pub time: Duration,
}

impl Default for DrainBudget {
    fn default() -> Self {
        DrainBudget {
            publications: limits::DRAIN_PUBLICATIONS,
            bytes: limits::DRAIN_BYTES,
            time: Duration::from_micros(limits::DRAIN_MICROS),
        }
    }
}

/// Why a drain slice stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStop {
    /// The bus is empty: nothing left, no re-arm.
    Empty,
    Publications,
    Bytes,
    Time,
    /// Pending terminal input preempted the slice.
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    pub applied: usize,
    pub bytes: usize,
    pub stop: DrainStop,
    /// The active root's partition (or the shared health row) changed, so the
    /// visible Problems list must be rebuilt — once, for this slice.
    pub visible_changed: bool,
    /// The slice stopped with work queued and re-armed the bridge wake.
    pub rearmed: bool,
}

/// Apply one bounded slice of queued diagnostics to `store`.
///
/// Order matters: loss marks are taken *before* publications, so a mark can
/// never land after (and wrongly shadow) the newer publication that
/// superseded it. The first publication is always applied (progress under a
/// steady input stream); after that the slice stops at the publication/byte/
/// time budget or when `input_pending` reports queued terminal input, finishing
/// at most one publication past a limit. A slice that stops with work left
/// re-arms the bus's single wake obligation — never a timer.
pub fn drain_diagnostics(
    rx: &DiagnosticsReceiver,
    store: &mut LspDiagnostics,
    active_root: &Path,
    budget: DrainBudget,
    mut elapsed: impl FnMut() -> Duration,
    mut input_pending: impl FnMut() -> bool,
    mut on_item: impl FnMut(),
) -> DrainOutcome {
    let mut visible_changed = false;
    if let Some(active) = rx.take_retirements() {
        visible_changed |= store.retain_streams(&active).contains(active_root);
    }
    let marks = rx.take_loss_marks();
    if !marks.is_empty() {
        visible_changed = true; // the shared health row reflects active loss
        store.mark_incomplete(marks.documents);
        store.mark_streams_incomplete(marks.streams);
    }
    let mut applied = 0usize;
    let mut bytes = 0usize;
    let mut stop = DrainStop::Empty;
    loop {
        if applied > 0 {
            if applied >= budget.publications {
                stop = DrainStop::Publications;
                break;
            }
            if bytes >= budget.bytes {
                stop = DrainStop::Bytes;
                break;
            }
            if elapsed() >= budget.time {
                stop = DrainStop::Time;
                break;
            }
            if input_pending() {
                stop = DrainStop::Input;
                break;
            }
        }
        let Ok(pd) = rx.try_recv() else {
            break;
        };
        on_item();
        bytes = bytes.saturating_add(thegn_svc::lsp::published_diagnostics_bytes(&pd));
        applied += 1;
        visible_changed |= pd.root == active_root;
        store.apply(pd);
    }
    let health = rx.take_health();
    if health.has_findings() {
        store.record_health(health);
        visible_changed = true;
    }
    let rearmed = stop != DrainStop::Empty && rx.rearm_if_pending();
    DrainOutcome {
        applied,
        bytes,
        stop,
        visible_changed,
        rearmed,
    }
}

// ─── the retained store ──────────────────────────────────────────────────────

/// Persistent store of LSP-pushed diagnostics, partitioned by the worktree
/// root that produced them and keyed by `(server stream, file)` within each
/// root. Survives model-hydration swaps (which only carry git/db state) so the
/// Problems panel keeps showing them; re-merged into the rendered list when
/// the active partition changes and on every swap. The partition is what keeps
/// one workspace's warm servers (they stay alive across tab switches) from
/// bleeding diagnostics into another's panel.
///
/// Bounds: `MAX_ROOTS` roots, `MAX_FILES_PER_ROOT` files per root,
/// `MAX_DIAGNOSTICS` items per file and `MAX_RETAINED_BYTES` in total, counted
/// over every owned byte (keys, the per-item duplicated file path, strings and
/// inline struct sizes) and checked on borrowed lengths *before* any item is
/// built. A refused publication leaves the previous data in place but marks
/// the document as having lost state — that mark ends only when a complete
/// publication for the document is actually committed, or its stream retires.
#[derive(Debug, Default)]
pub struct LspDiagnostics {
    by_root: HashMap<PathBuf, RootDiagnostics>,
    retained_bytes: usize,
    health: LspHealth,
    incomplete: HashSet<StoreFileKey>,
    incomplete_streams: HashSet<StoreStreamKey>,
    /// A loss that could not be tracked by document or stream (both mark sets
    /// full). Ends only when every other mark has ended.
    loss_overflow: bool,
}

#[derive(Debug, Default)]
struct RootDiagnostics {
    files: BTreeMap<StoreFileKey, Vec<DiagnosticItem>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct StoreFileKey {
    root: PathBuf,
    server_identity: String,
    generation: u64,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct StoreStreamKey {
    root: PathBuf,
    server_identity: String,
    generation: u64,
}

impl StoreFileKey {
    fn stream(&self) -> StreamKey {
        StreamKey {
            root: self.root.clone(),
            server_identity: self.server_identity.clone(),
            generation: self.generation,
        }
    }

    #[cfg(test)]
    fn is_stream(&self, stream: &StoreStreamKey) -> bool {
        self.root == stream.root
            && self.server_identity == stream.server_identity
            && self.generation == stream.generation
    }
}

impl StoreStreamKey {
    fn stream(&self) -> StreamKey {
        StreamKey {
            root: self.root.clone(),
            server_identity: self.server_identity.clone(),
            generation: self.generation,
        }
    }
}

const LSP_SOURCE_PREFIX: &str = "lsp:";
const LSP_DEFAULT_SOURCE: &str = "server";
const MAX_INCOMPLETE_STREAMS: usize = limits::MAX_ROOTS * limits::MAX_SERVERS_PER_ROOT;

fn store_key_bytes(key: &StoreFileKey) -> usize {
    key.root
        .as_os_str()
        .as_encoded_bytes()
        .len()
        .saturating_add(key.server_identity.len())
        .saturating_add(key.path.len())
        .saturating_add(std::mem::size_of::<StoreFileKey>())
}

fn root_bytes(root: &Path) -> usize {
    root.as_os_str()
        .as_encoded_bytes()
        .len()
        .saturating_add(std::mem::size_of::<PathBuf>())
        .saturating_add(std::mem::size_of::<RootDiagnostics>())
}

/// Upper bound of one panel item built from `d` for `file`, on borrowed
/// lengths (sanitization only removes characters).
fn projected_item_bytes(file: &str, d: &LspDiagnostic) -> usize {
    std::mem::size_of::<DiagnosticItem>()
        .saturating_add(file.len())
        .saturating_add(d.message.len())
        .saturating_add(LSP_SOURCE_PREFIX.len())
        .saturating_add(
            d.source
                .as_ref()
                .map_or(LSP_DEFAULT_SOURCE.len(), String::len),
        )
        .saturating_add(d.code.as_ref().map_or(0, String::len))
}

fn item_bytes(item: &DiagnosticItem) -> usize {
    std::mem::size_of::<DiagnosticItem>()
        .saturating_add(item.file.len())
        .saturating_add(item.message.len())
        .saturating_add(item.source.len())
        .saturating_add(item.code.as_ref().map_or(0, String::len))
}

/// Exact cost of one retained file entry: its map key, the Vec header and
/// every item (each carrying its own copy of the file path).
fn entry_bytes(key: &StoreFileKey, items: &[DiagnosticItem]) -> usize {
    items.iter().fold(
        store_key_bytes(key).saturating_add(std::mem::size_of::<Vec<DiagnosticItem>>()),
        |n, item| n.saturating_add(item_bytes(item)),
    )
}

/// The retained footprint, maintained vs recomputed — tests only.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoreFootprint {
    bytes: usize,
    recomputed: usize,
    roots: usize,
    files: usize,
}

impl LspDiagnostics {
    pub fn new() -> Self {
        LspDiagnostics::default()
    }

    /// Apply a server's latest diagnostics for one document, filed under the
    /// originating client's worktree root (stamped on the message). A
    /// complete empty set clears that file; an incomplete (malformed or
    /// truncated) empty set never masquerades as a clear.
    pub fn apply(&mut self, pd: PublishedDiagnostics) {
        if pd.root.as_os_str().is_empty()
            || pd.root.as_os_str().as_encoded_bytes().len() > limits::MAX_IDENTITY_BYTES
            || pd.server_identity.len() > limits::MAX_IDENTITY_BYTES
            || pd.path.len() > limits::MAX_IDENTITY_BYTES
        {
            self.health.invalid = self.health.invalid.saturating_add(1);
            return;
        }
        let file = relativize(&pd.path, &pd.root);
        let key = StoreFileKey {
            root: pd.root,
            server_identity: pd.server_identity,
            generation: pd.generation,
            path: file,
        };
        if pd.diagnostics.is_empty() {
            if pd.complete {
                self.remove_file(&key);
                self.incomplete.remove(&key);
            } else {
                self.health.incomplete = self.health.incomplete.saturating_add(1);
                self.remember_incomplete(key);
            }
            return;
        }

        // ── admission, on borrowed lengths, before building anything ──
        let root_state = self.by_root.get(&key.root);
        let old_bytes = root_state
            .and_then(|root| root.files.get(&key))
            .map(|items| entry_bytes(&key, items));
        let new_root_bytes = if root_state.is_none() {
            root_bytes(&key.root)
        } else {
            0
        };
        let projected = pd.diagnostics.iter().fold(
            store_key_bytes(&key).saturating_add(std::mem::size_of::<Vec<DiagnosticItem>>()),
            |n, d| n.saturating_add(projected_item_bytes(&key.path, d)),
        );
        let over_roots = root_state.is_none() && self.by_root.len() >= limits::MAX_ROOTS;
        let over_files = old_bytes.is_none()
            && root_state.is_some_and(|root| root.files.len() >= limits::MAX_FILES_PER_ROOT);
        let over_bytes = self
            .retained_bytes
            .saturating_sub(old_bytes.unwrap_or(0))
            .saturating_add(projected)
            .saturating_add(new_root_bytes)
            > limits::MAX_RETAINED_BYTES;
        if pd.diagnostics.len() > limits::MAX_DIAGNOSTICS || over_roots || over_files || over_bytes
        {
            self.refuse(key);
            return;
        }

        let items: Vec<DiagnosticItem> = pd
            .diagnostics
            .into_iter()
            .map(|d| to_panel_item(&key.path, d))
            .collect();
        let bytes = entry_bytes(&key, &items);
        debug_assert!(bytes <= projected, "projection must bound the build");
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(old_bytes.unwrap_or(0))
            .saturating_add(bytes)
            .saturating_add(new_root_bytes);
        let complete = pd.complete;
        let root = self.by_root.entry(key.root.clone()).or_default();
        root.files.insert(key.clone(), items);
        if complete {
            self.incomplete.remove(&key);
        } else {
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            self.remember_incomplete(key);
        }
    }

    /// Replace the LSP-sourced entries in `dst` with the current store's
    /// entries **for `root` only**, keeping any non-LSP (task-output)
    /// diagnostics, then re-sort by severity. Foreign roots' diagnostics stay
    /// in the store but never render.
    pub fn merge_into(&self, root: &Path, dst: &mut Vec<DiagnosticItem>) {
        dst.retain(|d| !d.source.starts_with(LSP_SOURCE_PREFIX));
        if let Some(files) = self.by_root.get(root) {
            for items in files.files.values() {
                dst.extend(items.iter().cloned());
            }
        }
        if self.health.has_findings() || self.active_loss() > 0 {
            dst.push(DiagnosticItem {
                file: String::new(),
                line: 0,
                col: None,
                severity: Severity::Warning,
                message: format!("{} active={}", self.health.summary(), self.active_loss()),
                source: "lsp:health".to_string(),
                code: None,
            });
        }
        dst.sort_by_key(|d| d.severity as u8);
    }

    /// Documents/streams whose latest state is currently known to be lost.
    fn active_loss(&self) -> usize {
        self.incomplete
            .len()
            .saturating_add(self.incomplete_streams.len())
            .saturating_add(usize::from(self.loss_overflow))
    }

    /// Drop everything a closed worktree's servers pushed (memory hygiene —
    /// rendering already ignores non-active roots).
    #[allow(dead_code)] // exercised by tests; the loop evicts via `retain_roots`
    pub fn evict_root(&mut self, root: &Path) {
        self.retain_roots(|candidate| candidate != root);
    }

    /// Keep only the roots `keep` approves — called on the periodic model swap
    /// with the set of open worktree tabs, so closed/deleted worktrees' entries
    /// don't accumulate for the life of the process.
    pub fn retain_roots(&mut self, keep: impl Fn(&Path) -> bool) {
        let removed = self
            .by_root
            .iter()
            .filter(|(root, _)| !keep(root))
            .map(|(root, state)| {
                state
                    .files
                    .iter()
                    .fold(root_bytes(root), |n, (key, items)| {
                        n.saturating_add(entry_bytes(key, items))
                    })
            })
            .fold(0usize, usize::saturating_add);
        self.by_root.retain(|root, _| keep(root));
        self.retained_bytes = self.retained_bytes.saturating_sub(removed);
        self.incomplete.retain(|key| keep(&key.root));
        self.incomplete_streams.retain(|key| keep(&key.root));
        self.settle_overflow();
    }

    /// Keep only data from streams that are still registered on the bus
    /// (`active`). This is the only path by which a stream's retained
    /// diagnostics and loss marks end besides explicit clears: the evidence is
    /// the supervisor's registration set, never a publication's own numbers.
    /// Returns the roots whose partitions changed.
    pub fn retain_streams(&mut self, active: &HashSet<StreamKey>) -> HashSet<PathBuf> {
        let mut changed = HashSet::new();
        let mut released = 0usize;
        let mut emptied = Vec::new();
        for (root, state) in &mut self.by_root {
            let before = state.files.len();
            state.files.retain(|key, items| {
                let keep = active.contains(&key.stream());
                if !keep {
                    released = released.saturating_add(entry_bytes(key, items));
                }
                keep
            });
            if state.files.len() != before {
                changed.insert(root.clone());
            }
            if state.files.is_empty() {
                emptied.push(root.clone());
            }
        }
        for root in emptied {
            self.by_root.remove(&root);
            released = released.saturating_add(root_bytes(&root));
        }
        self.retained_bytes = self.retained_bytes.saturating_sub(released);
        self.incomplete.retain(|key| active.contains(&key.stream()));
        self.incomplete_streams
            .retain(|key| active.contains(&key.stream()));
        self.settle_overflow();
        changed
    }

    #[allow(dead_code)] // exercised by tests; the loop-side caller was removed
    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty() && !self.health.has_findings() && self.active_loss() == 0
    }

    pub fn record_health(&mut self, health: LspHealth) {
        self.health.saturating_add(health);
    }

    /// Documents whose latest publication the bus had to drop. Health was
    /// already counted by the bus.
    pub fn mark_incomplete(&mut self, keys: impl IntoIterator<Item = DiagnosticKey>) {
        for key in keys {
            let path = relativize(&key.path, &key.root);
            self.remember_incomplete(StoreFileKey {
                root: key.root,
                server_identity: key.server_identity,
                generation: key.generation,
                path,
            });
        }
    }

    /// Streams with untracked losses; they stay incomplete until they retire.
    pub fn mark_streams_incomplete(&mut self, streams: impl IntoIterator<Item = StreamKey>) {
        for stream in streams {
            self.remember_incomplete_stream(StoreStreamKey {
                root: stream.root,
                server_identity: stream.server_identity,
                generation: stream.generation,
            });
        }
    }

    fn refuse(&mut self, key: StoreFileKey) {
        self.health.dropped = self.health.dropped.saturating_add(1);
        self.health.incomplete = self.health.incomplete.saturating_add(1);
        self.remember_incomplete(key);
    }

    fn remember_incomplete(&mut self, key: StoreFileKey) {
        if self.incomplete.contains(&key) {
            return;
        }
        if self.incomplete.len() < limits::MAX_LOSS_MARKS {
            self.incomplete.insert(key);
            return;
        }
        // Mark set full: escalate to the document's stream (never evict an
        // existing mark — that would silently turn a loss into "clean").
        self.remember_incomplete_stream(StoreStreamKey {
            root: key.root,
            server_identity: key.server_identity,
            generation: key.generation,
        });
    }

    fn remember_incomplete_stream(&mut self, stream: StoreStreamKey) {
        if self.incomplete_streams.contains(&stream) {
            return;
        }
        if self.incomplete_streams.len() < MAX_INCOMPLETE_STREAMS {
            self.incomplete_streams.insert(stream);
        } else {
            self.loss_overflow = true;
        }
    }

    fn settle_overflow(&mut self) {
        if self.incomplete.is_empty() && self.incomplete_streams.is_empty() {
            self.loss_overflow = false;
        }
    }

    fn remove_file(&mut self, key: &StoreFileKey) {
        let Some(root) = self.by_root.get_mut(&key.root) else {
            return;
        };
        let Some(items) = root.files.remove(key) else {
            return;
        };
        let mut released = entry_bytes(key, &items);
        if root.files.is_empty() {
            self.by_root.remove(&key.root);
            released = released.saturating_add(root_bytes(&key.root));
        }
        self.retained_bytes = self.retained_bytes.saturating_sub(released);
    }

    #[cfg(test)]
    fn footprint(&self) -> StoreFootprint {
        let recomputed = self
            .by_root
            .iter()
            .map(|(root, state)| {
                state
                    .files
                    .iter()
                    .fold(root_bytes(root), |n, (key, items)| {
                        n.saturating_add(entry_bytes(key, items))
                    })
            })
            .fold(0usize, usize::saturating_add);
        StoreFootprint {
            bytes: self.retained_bytes,
            recomputed,
            roots: self.by_root.len(),
            files: self.by_root.values().map(|root| root.files.len()).sum(),
        }
    }

    #[cfg(test)]
    fn is_incomplete(&self, root: &str, identity: &str, generation: u64, file: &str) -> bool {
        let stream = StoreStreamKey {
            root: PathBuf::from(root),
            server_identity: identity.to_string(),
            generation,
        };
        self.loss_overflow
            || self.incomplete_streams.contains(&stream)
            || self
                .incomplete
                .iter()
                .any(|key| key.is_stream(&stream) && key.path == file)
    }
}

/// Convert one svc diagnostic to a panel item (source tagged `lsp:<source>`).
fn to_panel_item(file: &str, d: thegn_svc::lsp::LspDiagnostic) -> DiagnosticItem {
    DiagnosticItem {
        file: file.to_string(),
        line: d.line as u64 + 1, // LSP is 0-based; the panel shows 1-based
        col: Some(d.character as u64 + 1),
        severity: match d.severity {
            LspSeverity::Error => Severity::Error,
            LspSeverity::Warning => Severity::Warning,
            LspSeverity::Info => Severity::Info,
            LspSeverity::Hint => Severity::Hint,
        },
        message: thegn_svc::lsp::sanitize_for_terminal(&d.message),
        source: format!(
            "{LSP_SOURCE_PREFIX}{}",
            thegn_svc::lsp::sanitize_for_terminal(
                d.source.as_deref().unwrap_or(LSP_DEFAULT_SOURCE)
            )
        ),
        code: d
            .code
            .map(|code| thegn_svc::lsp::sanitize_for_terminal(&code)),
    }
}

/// Strip `root` from `path` when `path` is under it; otherwise return `path`.
fn relativize(path: &str, root: &Path) -> String {
    if let Ok(rel) = Path::new(path).strip_prefix(root) {
        return rel.to_string_lossy().into_owned();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_svc::lsp::LspDiagnostic;

    fn pd(root: &str, path: &str, diags: Vec<LspDiagnostic>) -> PublishedDiagnostics {
        PublishedDiagnostics {
            root: PathBuf::from(root),
            path: path.to_string(),
            diagnostics: diags,
            server_identity: "test".into(),
            generation: 1,
            sequence: 1,
            complete: true,
        }
    }

    fn diag(line: u32, sev: LspSeverity, msg: &str) -> LspDiagnostic {
        LspDiagnostic {
            line,
            character: 0,
            severity: sev,
            message: msg.to_string(),
            code: None,
            source: Some("rustc".into()),
        }
    }

    #[test]
    fn apply_relativizes_and_converts() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/proj",
            "/proj/src/lib.rs",
            vec![diag(5, LspSeverity::Error, "boom")],
        ));
        let mut dst = Vec::new();
        store.merge_into(Path::new("/proj"), &mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].file, "src/lib.rs");
        assert_eq!(dst[0].line, 6); // 0-based 5 → 1-based 6
        assert_eq!(dst[0].source, "lsp:rustc");
        assert_eq!(dst[0].severity, Severity::Error);
    }

    #[test]
    fn empty_set_clears_a_file() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/p",
            "/p/x.rs",
            vec![diag(0, LspSeverity::Warning, "w")],
        ));
        assert!(!store.is_empty());
        store.apply(pd("/p", "/p/x.rs", vec![]));
        assert!(store.is_empty());
    }

    #[test]
    fn merge_is_partitioned_by_root() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/a",
            "/a/x.rs",
            vec![diag(1, LspSeverity::Error, "a-err")],
        ));
        store.apply(pd(
            "/b",
            "/b/y.rs",
            vec![diag(2, LspSeverity::Warning, "b-warn")],
        ));

        // Merging for root /a yields only /a's diagnostics.
        let mut dst = Vec::new();
        store.merge_into(Path::new("/a"), &mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].message, "a-err");

        // Switching to /b swaps the set — /a's entry (lsp-sourced) is dropped.
        store.merge_into(Path::new("/b"), &mut dst);
        assert_eq!(dst.len(), 1);
        assert_eq!(dst[0].message, "b-warn");

        // Evicting a root removes its partition entirely.
        store.evict_root(Path::new("/b"));
        store.merge_into(Path::new("/b"), &mut dst);
        assert!(dst.is_empty());
        assert!(!store.is_empty()); // /a still stored
    }

    #[test]
    fn merge_keeps_task_diags_and_replaces_lsp() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/p",
            "/p/a.rs",
            vec![diag(1, LspSeverity::Error, "lsp-err")],
        ));

        // A pre-existing task diagnostic plus a stale LSP one.
        let mut dst = vec![
            DiagnosticItem {
                file: "b.rs".into(),
                line: 2,
                col: None,
                severity: Severity::Warning,
                message: "task-warn".into(),
                source: "cargo clippy".into(),
                code: None,
            },
            DiagnosticItem {
                file: "old.rs".into(),
                line: 9,
                col: None,
                severity: Severity::Hint,
                message: "stale-lsp".into(),
                source: "lsp:rustc".into(),
                code: None,
            },
        ];
        store.merge_into(Path::new("/p"), &mut dst);

        // The task diagnostic survives; the stale LSP one is gone; ours is added.
        assert!(dst.iter().any(|d| d.source == "cargo clippy"));
        assert!(!dst.iter().any(|d| d.message == "stale-lsp"));
        assert!(dst.iter().any(|d| d.message == "lsp-err"));
        // Sorted by severity → Error (0) before Warning (1).
        assert_eq!(dst[0].severity, Severity::Error);
    }

    #[test]
    fn disabled_supervisor_yields_unavailable() {
        let mut cfg = Config::default();
        cfg.lsp.enabled = false;
        let sup = LspSupervisor::from_config(&cfg);
        let res = sup.handle().client(Path::new("/tmp"), "rust");
        assert_eq!(res.err(), Some(LspError::NotAvailable));
    }

    #[test]
    fn missing_server_is_unavailable_and_cached() {
        // Override Rust to a binary that cannot exist → NotAvailable, cached.
        let mut cfg = Config::default();
        cfg.lsp.servers = vec![thegn_core::config::LspServerConfig {
            lang: "rust".into(),
            command: "/nonexistent/definitely-not-a-server".into(),
            args: vec![],
            extensions: vec![],
            language_id: None,
        }];
        let sup = LspSupervisor::from_config(&cfg);
        let inner = sup.handle();
        // An explicit override command is trusted, so it attempts to spawn and
        // fails with Spawn (not NotAvailable) — still an error, not a panic.
        let res = inner.client(Path::new("/tmp"), "rust");
        assert!(res.is_err());
    }

    #[test]
    fn unregistered_extension_resolves_to_no_key() {
        let cfg = Config::default();
        let inner = LspSupervisor::from_config(&cfg).handle();
        assert_eq!(inner.resolve_key("src/lib.rs").as_deref(), Some("rust"));
        assert_eq!(inner.resolve_key("README.md"), None);
    }

    #[test]
    fn registry_key_resolves_for_a_non_builtin_language() {
        let mut cfg = Config::default();
        cfg.lsp.servers = vec![thegn_core::config::LspServerConfig {
            lang: "zig".into(),
            command: "zls".into(),
            args: vec![],
            extensions: vec!["zig".into(), "zon".into()],
            language_id: None,
        }];
        let inner = LspSupervisor::from_config(&cfg).handle();
        assert_eq!(inner.resolve_key("main.zig").as_deref(), Some("zig"));
    }

    #[test]
    fn same_document_streams_do_not_clear_each_other() {
        let mut store = LspDiagnostics::new();
        let mut rust = pd("/p", "/p/a.rs", vec![diag(0, LspSeverity::Error, "rust")]);
        rust.server_identity = "rust".into();
        let mut clang = pd(
            "/p",
            "/p/a.rs",
            vec![diag(1, LspSeverity::Warning, "clang")],
        );
        clang.server_identity = "clang".into();
        store.apply(rust.clone());
        store.apply(clang);
        store.apply(PublishedDiagnostics {
            diagnostics: vec![],
            ..rust
        });
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert_eq!(
            dst.iter()
                .filter(|item| item.source.starts_with("lsp:"))
                .count(),
            1
        );
        assert_eq!(dst[0].message, "clang");
    }

    #[test]
    fn retained_store_caps_files_and_sanitizes_display_text() {
        let mut store = LspDiagnostics::new();
        for index in 0..=thegn_svc::lsp::limits::MAX_FILES_PER_ROOT {
            store.apply(pd(
                "/p",
                &format!("/p/{index}.rs"),
                vec![diag(0, LspSeverity::Error, "bad\u{1b}]0;title\u{7} text")],
            ));
        }
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(dst.len() <= thegn_svc::lsp::limits::MAX_FILES_PER_ROOT + 1);
        assert!(
            dst.iter()
                .all(|item| !item.message.chars().any(char::is_control))
        );
        assert!(dst.iter().any(|item| item.source == "lsp:health"));
    }

    // ── THE-336: bounded store, lifecycle and budgeted drain ────────────────

    fn fake_client(
        root: &Path,
        key: &str,
        diagnostics: DiagnosticsSender,
        generation: u64,
    ) -> Result<Arc<LspClient>, LspError> {
        Ok(Arc::new(LspClient::from_io_with_identity(
            Box::new(std::io::empty()),
            Box::new(std::io::sink()),
            key,
            root,
            diagnostics,
            key.to_string(),
            generation,
        )))
    }

    fn enabled_inner() -> (Arc<LspInner>, DiagnosticsReceiver) {
        let mut cfg = Config::default();
        cfg.lsp.enabled = true;
        let mut sup = LspSupervisor::from_config(&cfg);
        let rx = sup.take_diagnostics_rx().unwrap();
        (sup.handle(), rx)
    }

    fn stamped(
        root: &str,
        identity: &str,
        generation: u64,
        sequence: u64,
        path: &str,
        diags: Vec<LspDiagnostic>,
    ) -> PublishedDiagnostics {
        PublishedDiagnostics {
            server_identity: identity.into(),
            generation,
            sequence,
            ..pd(root, path, diags)
        }
    }

    #[track_caller]
    fn assert_store_conserved(store: &LspDiagnostics) -> StoreFootprint {
        let f = store.footprint();
        assert_eq!(f.bytes, f.recomputed, "store bytes drifted: {f:?}");
        assert!(f.bytes <= limits::MAX_RETAINED_BYTES);
        assert!(f.roots <= limits::MAX_ROOTS);
        f
    }

    #[test]
    fn more_than_max_roots_sequential_open_close_keeps_working() {
        let (inner, rx) = enabled_inner();
        for index in 0..(limits::MAX_ROOTS * 3) {
            let root = PathBuf::from(format!("/wt/{index}"));
            let client = inner
                .client_with(&root, "rust", fake_client)
                .unwrap_or_else(|e| panic!("open #{index}: {e}"));
            assert_eq!(client.root(), root.as_path());
            // A negative-cache slot for another language on the same root.
            assert_eq!(
                inner
                    .client_with(&root, "zig", |_, _, _, _| Err(LspError::NotAvailable))
                    .err(),
                Some(LspError::NotAvailable)
            );
            // The worktree closes: both slots and the registration are released.
            assert_eq!(inner.reconcile_roots(&HashSet::new()), 2);
        }
        assert_eq!(rx.footprint().active_streams, 0);
        assert_eq!(rx.footprint().metadata_bytes, 0);
        assert!(inner.clients.lock().unwrap().is_empty());
    }

    #[test]
    fn supervisor_bounds_roots_and_servers_per_root() {
        let (inner, _rx) = enabled_inner();
        for index in 0..limits::MAX_ROOTS {
            inner
                .client_with(Path::new(&format!("/r/{index}")), "rust", fake_client)
                .unwrap();
        }
        assert!(matches!(
            inner.client_with(Path::new("/r/extra"), "rust", fake_client),
            Err(LspError::Bounded(_))
        ));
        let long = "k".repeat(limits::MAX_IDENTITY_BYTES + 1);
        assert!(matches!(
            inner.client_with(Path::new("/r/0"), &long, fake_client),
            Err(LspError::Bounded(_))
        ));
    }

    #[test]
    fn close_reopen_purges_only_the_retired_stream_from_the_store() {
        let (inner, rx) = enabled_inner();
        let root = Path::new("/p");
        let rust = inner.client_with(root, "rust", fake_client).unwrap();
        let clang = inner.client_with(root, "clang", fake_client).unwrap();
        let (g_rust, g_clang) = (rust.generation(), clang.generation());
        let tx = inner.diag_tx.clone();
        tx.publish(stamped(
            "/p",
            "rust",
            g_rust,
            1,
            "/p/a.c",
            vec![diag(0, LspSeverity::Error, "rust-old")],
        ));
        tx.publish(stamped(
            "/p",
            "clang",
            g_clang,
            1,
            "/p/a.c",
            vec![diag(1, LspSeverity::Warning, "clang")],
        ));
        let mut store = LspDiagnostics::new();
        let budget = DrainBudget::default();
        drain_diagnostics(
            &rx,
            &mut store,
            root,
            budget,
            || Duration::ZERO,
            || false,
            || {},
        );
        assert_eq!(store.footprint().files, 2);

        // Restart rust: old generation retired, clang untouched.
        drop(rust);
        inner.close(root, "rust");
        let rust2 = inner.client_with(root, "rust", fake_client).unwrap();
        assert!(rust2.generation() > g_rust);
        // A late old-generation publication is stale at the bus.
        tx.publish(stamped(
            "/p",
            "rust",
            g_rust,
            99,
            "/p/a.c",
            vec![diag(0, LspSeverity::Error, "late")],
        ));
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            root,
            budget,
            || Duration::ZERO,
            || false,
            || {},
        );
        assert!(
            outcome.visible_changed,
            "retirement changed the active partition"
        );
        let mut dst = Vec::new();
        store.merge_into(root, &mut dst);
        let messages: Vec<&str> = dst
            .iter()
            .filter(|d| d.source != "lsp:health")
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(messages, vec!["clang"]);
        assert_eq!(store.health.stale, 1);
        assert_store_conserved(&store);
    }

    #[test]
    fn drain_stops_at_eight_publications_and_rearms() {
        let (inner, rx) = enabled_inner();
        let client = inner
            .client_with(Path::new("/p"), "rust", fake_client)
            .unwrap();
        let g = client.generation();
        for seq in 1..=20 {
            inner.diag_tx.publish(stamped(
                "/p",
                "rust",
                g,
                seq,
                &format!("/p/{seq}.rs"),
                vec![],
            ));
        }
        assert!(
            rx.wait(),
            "consume the admission wake so the re-arm is observable"
        );
        let mut store = LspDiagnostics::new();
        let mut items = 0;
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || items += 1,
        );
        assert_eq!(outcome.applied, limits::DRAIN_PUBLICATIONS);
        assert_eq!(items, limits::DRAIN_PUBLICATIONS);
        assert_eq!(outcome.stop, DrainStop::Publications);
        assert!(
            outcome.rearmed && rx.wake_pending(),
            "remaining work re-arms the wake"
        );
        // Second and third slices finish the queue; the last one does not re-arm.
        let second = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        );
        assert_eq!(second.applied, 8);
        assert!(rx.wait());
        let third = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        );
        assert_eq!(
            (third.applied, third.stop, third.rearmed),
            (4, DrainStop::Empty, false)
        );
        assert!(!rx.wake_pending(), "an empty bus leaves no wake behind");
    }

    #[test]
    fn drain_byte_budget_overshoots_by_at_most_one_publication() {
        let (inner, rx) = enabled_inner();
        let client = inner
            .client_with(Path::new("/p"), "rust", fake_client)
            .unwrap();
        let g = client.generation();
        let big = "b".repeat(100 * 1024);
        for seq in 1..=6 {
            inner.diag_tx.publish(stamped(
                "/p",
                "rust",
                g,
                seq,
                &format!("/p/{seq}.rs"),
                vec![diag(0, LspSeverity::Error, &big)],
            ));
        }
        let mut store = LspDiagnostics::new();
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        );
        assert_eq!(outcome.stop, DrainStop::Bytes);
        assert_eq!(
            outcome.applied, 3,
            "100 KiB ×3 crosses 256 KiB on the third"
        );
        assert!(outcome.bytes < limits::DRAIN_BYTES + limits::MAX_DIAGNOSTIC_BYTES);
        assert!(outcome.rearmed);
    }

    #[test]
    fn drain_time_budget_uses_the_injected_clock() {
        let (inner, rx) = enabled_inner();
        let client = inner
            .client_with(Path::new("/p"), "rust", fake_client)
            .unwrap();
        let g = client.generation();
        for seq in 1..=5 {
            inner.diag_tx.publish(stamped(
                "/p",
                "rust",
                g,
                seq,
                &format!("/p/{seq}.rs"),
                vec![],
            ));
        }
        let mut store = LspDiagnostics::new();
        // Each clock read advances 1 ms of logical time: the 2 ms budget
        // admits the first item unconditionally, then one more.
        let mut now = Duration::ZERO;
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || {
                now += Duration::from_millis(1);
                now
            },
            || false,
            || {},
        );
        assert_eq!((outcome.applied, outcome.stop), (2, DrainStop::Time));
        assert!(outcome.rearmed);
    }

    #[test]
    fn pending_input_preempts_between_items_but_first_item_progresses() {
        let (inner, rx) = enabled_inner();
        let client = inner
            .client_with(Path::new("/p"), "rust", fake_client)
            .unwrap();
        let g = client.generation();
        for seq in 1..=5 {
            inner.diag_tx.publish(stamped(
                "/p",
                "rust",
                g,
                seq,
                &format!("/p/{seq}.rs"),
                vec![],
            ));
        }
        let mut store = LspDiagnostics::new();
        let mut slices = 0;
        while rx.has_pending() {
            let outcome = drain_diagnostics(
                &rx,
                &mut store,
                Path::new("/p"),
                DrainBudget::default(),
                || Duration::ZERO,
                || true,
                || {},
            );
            assert_eq!(
                outcome.applied, 1,
                "one item per slice under continuous input"
            );
            slices += 1;
        }
        assert_eq!(slices, 5, "LSP still progresses while input streams");
    }

    #[test]
    fn inactive_root_publications_do_not_trigger_a_visible_rebuild() {
        let (inner, rx) = enabled_inner();
        let other = inner
            .client_with(Path::new("/other"), "rust", fake_client)
            .unwrap();
        inner.diag_tx.publish(stamped(
            "/other",
            "rust",
            other.generation(),
            1,
            "/other/x.rs",
            vec![diag(0, LspSeverity::Error, "x")],
        ));
        let mut store = LspDiagnostics::new();
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/active"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        );
        assert_eq!(outcome.applied, 1);
        assert!(!outcome.visible_changed);
    }

    #[test]
    fn quiet_root_progresses_across_budgeted_slices_under_a_flood() {
        let (inner, rx) = enabled_inner();
        let flood = inner
            .client_with(Path::new("/flood"), "rust", fake_client)
            .unwrap();
        let quiet = inner
            .client_with(Path::new("/quiet"), "rust", fake_client)
            .unwrap();
        for seq in 1..=200 {
            inner.diag_tx.publish(stamped(
                "/flood",
                "rust",
                flood.generation(),
                seq,
                &format!("/flood/{seq}"),
                vec![diag(0, LspSeverity::Error, "f")],
            ));
        }
        inner.diag_tx.publish(stamped(
            "/quiet",
            "rust",
            quiet.generation(),
            1,
            "/quiet/q.rs",
            vec![diag(0, LspSeverity::Error, "q")],
        ));
        let mut store = LspDiagnostics::new();
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/quiet"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        );
        assert!(
            outcome.visible_changed,
            "the quiet (active) root landed in the first slice"
        );
        let mut dst = Vec::new();
        store.merge_into(Path::new("/quiet"), &mut dst);
        assert!(dst.iter().any(|d| d.message == "q"));
    }

    #[test]
    fn refused_publication_keeps_old_data_marked_incomplete_until_complete_replacement() {
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/p",
            "/p/a.rs",
            vec![diag(0, LspSeverity::Error, "old")],
        ));
        let too_many: Vec<_> = (0..=limits::MAX_DIAGNOSTICS as u32)
            .map(|line| diag(line, LspSeverity::Error, "n"))
            .collect();
        store.apply(pd("/p", "/p/a.rs", too_many));
        assert!(
            store.is_incomplete("/p", "test", 1, "a.rs"),
            "refusal is active loss"
        );
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(dst.iter().any(|d| d.message == "old"));
        assert!(
            dst.iter()
                .any(|d| d.source == "lsp:health" && d.message.contains("active=1"))
        );

        // An incomplete (truncated) publication replaces data but stays incomplete.
        let mut partial = pd(
            "/p",
            "/p/a.rs",
            vec![diag(0, LspSeverity::Error, "partial")],
        );
        partial.complete = false;
        store.apply(partial);
        assert!(store.is_incomplete("/p", "test", 1, "a.rs"));
        // An incomplete empty publication is never a clear.
        let mut bogus_clear = pd("/p", "/p/a.rs", vec![]);
        bogus_clear.complete = false;
        store.apply(bogus_clear);
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(dst.iter().any(|d| d.message == "partial"));
        // Only a complete publication commits the recovery.
        store.apply(pd(
            "/p",
            "/p/a.rs",
            vec![diag(0, LspSeverity::Error, "fresh")],
        ));
        assert!(!store.is_incomplete("/p", "test", 1, "a.rs"));
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(
            dst.iter().any(|d| d.message.contains("active=0")),
            "counters stay sticky, loss ends"
        );
        assert_store_conserved(&store);
    }

    #[test]
    fn store_bytes_are_conserved_and_capped_under_a_large_unique_flood() {
        let mut store = LspDiagnostics::new();
        let message = "m".repeat(limits::MAX_SCALAR_STRING_BYTES);
        let mut refused = 0;
        for index in 0..400 {
            let before = store.health.dropped;
            store.apply(pd(
                "/p",
                &format!("/p/{index}.rs"),
                (0..3)
                    .map(|l| diag(l, LspSeverity::Warning, &message))
                    .collect(),
            ));
            refused += usize::from(store.health.dropped > before);
            assert_store_conserved(&store);
        }
        assert!(refused > 0, "the 16 MiB cap engaged");
        // Replacements, clears, root eviction and stream retirement all conserve.
        store.apply(pd(
            "/p",
            "/p/0.rs",
            vec![diag(0, LspSeverity::Error, "small")],
        ));
        assert_store_conserved(&store);
        store.apply(pd("/p", "/p/1.rs", vec![]));
        assert_store_conserved(&store);
        store.apply(pd("/q", "/q/x.rs", vec![diag(0, LspSeverity::Error, "q")]));
        store.evict_root(Path::new("/q"));
        assert_store_conserved(&store);
        store.retain_streams(&HashSet::new());
        let f = assert_store_conserved(&store);
        assert_eq!((f.bytes, f.files, f.roots), (0, 0, 0));
        assert_eq!(
            store.active_loss(),
            0,
            "retired streams take their marks along"
        );
    }

    #[test]
    fn projected_admission_counts_duplicated_file_paths() {
        // A long file path is duplicated into every item; the preflight must
        // bound it before building (debug_assert in `apply` checks the build).
        let mut store = LspDiagnostics::new();
        let file = format!("/p/{}.rs", "d".repeat(limits::MAX_IDENTITY_BYTES - 16));
        let items: Vec<_> = (0..limits::MAX_DIAGNOSTICS as u32)
            .map(|line| diag(line, LspSeverity::Hint, "h"))
            .collect();
        store.apply(pd("/p", &file, items));
        let f = assert_store_conserved(&store);
        assert!(f.bytes >= limits::MAX_DIAGNOSTICS * (limits::MAX_IDENTITY_BYTES - 16));
    }
}
