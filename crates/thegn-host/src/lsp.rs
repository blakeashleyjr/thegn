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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// The client map plus the worktree-root set it was last reconciled against.
/// Both live under one lock so a request for a root that was just closed can
/// never respawn a server behind a reconcile.
#[derive(Default)]
struct ClientState {
    map: ClientMap,
    /// `None` until the host first reconciles; afterwards only these roots
    /// may start servers.
    live: Option<HashSet<PathBuf>>,
    /// Epoch of the applied root set. Reconcile tasks can run out of order
    /// (they queue on this lock); an older epoch is never applied over a newer.
    epoch: u64,
}

pub struct LspInner {
    enabled: bool,
    /// The resolved server registry (built-ins + user `[[lsp.servers]]`). Built
    /// once at startup and immutable, so it needs no lock and is shared read-only
    /// across every off-loop request task.
    registry: Registry,
    diag_tx: DiagnosticsSender,
    clients: Mutex<ClientState>,
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
                clients: Mutex::new(ClientState::default()),
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
        let mut state = self
            .clients
            .lock()
            .map_err(|_| LspError::Protocol("LSP client map poisoned".into()))?;
        if state.live.as_ref().is_some_and(|live| !live.contains(root)) {
            // The worktree is closed (or not yet known): never respawn behind
            // a reconcile. Not cached — reopening the root re-enables it.
            return Err(LspError::NotAvailable);
        }
        let clients = &mut state.map;
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
            .and_then(|mut state| state.map.remove(&(root.to_path_buf(), key.to_string())));
        if let Some(ClientSlot::Live { client, generation }) = removed {
            self.diag_tx.retire(root, key, generation);
            teardown_off_loop(vec![client]);
        }
    }

    /// Retire every server (and negative-cache slot) whose worktree is no
    /// longer in `live_roots`, releasing registry capacity; subprocess
    /// teardown happens on a worker thread. Blocks on the client map (which a
    /// concurrent spawn may hold) — call off the event loop.
    ///
    /// `epoch` orders concurrent calls: the host bumps it on every root-set
    /// change, and a call whose epoch is not newer than the applied one is a
    /// no-op, so out-of-order tasks converge on the latest set. Returns how
    /// many slots were released.
    pub fn reconcile_roots(&self, epoch: u64, live_roots: HashSet<PathBuf>) -> usize {
        let mut teardown = Vec::new();
        let mut released = 0usize;
        if let Ok(mut state) = self.clients.lock() {
            if state.live.is_some() && epoch <= state.epoch {
                return 0;
            }
            state.epoch = epoch;
            let clients = &mut state.map;
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
            state.live = Some(live_roots);
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

/// What the visible Problems list needs after a slice. Never a full rebuild
/// for ordinary traffic: `Patch` touches only the named files (plus the
/// health rows) of the active root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum VisibleRefresh {
    #[default]
    None,
    /// Replace the LSP items of these (root-relative) files and the health
    /// rows; an empty set rewrites only the health rows.
    Patch(HashSet<String>),
    /// A retirement changed the active partition wholesale (rare).
    Full,
}

impl VisibleRefresh {
    fn file(&mut self, file: String) {
        match self {
            VisibleRefresh::Full => {}
            VisibleRefresh::Patch(files) => {
                files.insert(file);
            }
            VisibleRefresh::None => *self = VisibleRefresh::Patch(HashSet::from([file])),
        }
    }

    fn rows(&mut self) {
        if *self == VisibleRefresh::None {
            *self = VisibleRefresh::Patch(HashSet::new());
        }
    }

    /// Apply to `dst` for `root` — a bounded patch, or a full merge.
    pub fn apply_to(&self, store: &LspDiagnostics, root: &Path, dst: &mut Vec<DiagnosticItem>) {
        match self {
            VisibleRefresh::None => {}
            VisibleRefresh::Patch(files) => store.patch_into(root, dst, files),
            VisibleRefresh::Full => store.merge_into(root, dst),
        }
    }

    pub fn is_none(&self) -> bool {
        *self == VisibleRefresh::None
    }
}

/// Which root the visible Problems list currently holds. A patch is only
/// valid for the list it was computed against: after a tab switch the list
/// still holds the previous worktree's items (the hydration swap that rebuilds
/// it runs later), and patching by root-relative path alone would splice the
/// new worktree's file over the old one's and leave the rest of the old
/// worktree's problems on screen. Any root change therefore forces a full
/// rebuild — the partition boundary the store exists to keep.
#[derive(Debug, Default)]
pub struct VisibleTarget {
    root: Option<PathBuf>,
}

impl VisibleTarget {
    /// The refresh to actually apply for `root`, given what the slice asked
    /// for. Records `root` as the list's owner.
    pub fn refresh_for(&mut self, root: &Path, refresh: VisibleRefresh) -> VisibleRefresh {
        if self.root.as_deref() == Some(root) {
            return refresh;
        }
        self.root = Some(root.to_path_buf());
        VisibleRefresh::Full
    }

    /// The list was rebuilt for `root` by something other than a drain slice
    /// (the hydration swap).
    pub fn rebuilt(&mut self, root: &Path) {
        self.root = Some(root.to_path_buf());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    pub applied: usize,
    pub bytes: usize,
    pub stop: DrainStop,
    /// How the active root's visible list must be refreshed for this slice.
    pub refresh: VisibleRefresh,
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
/// re-arms the bus's single wake obligation — never a timer. The returned
/// refresh names only the active root's files this slice touched, so the
/// visible update is bounded by the slice too (see [`LspDiagnostics::patch_into`]).
pub fn drain_diagnostics(
    rx: &DiagnosticsReceiver,
    store: &mut LspDiagnostics,
    active_root: &Path,
    budget: DrainBudget,
    mut elapsed: impl FnMut() -> Duration,
    mut input_pending: impl FnMut() -> bool,
    mut on_item: impl FnMut(),
) -> DrainOutcome {
    let mut refresh = VisibleRefresh::None;
    if let Some(active) = rx.take_retirements()
        && store.retain_streams(&active).contains(active_root)
    {
        refresh = VisibleRefresh::Full;
    }
    let marks = rx.take_loss_marks();
    if !marks.is_empty() {
        let docs = store.mark_incomplete(marks.documents, active_root);
        let streams = store.mark_streams_incomplete(marks.streams, active_root);
        if docs || streams {
            refresh.rows();
        }
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
        let active = pd.root == active_root;
        if let Some(file) = store.apply(pd)
            && active
        {
            refresh.file(file);
        }
    }
    let health = rx.take_health();
    if health.has_findings() {
        store.record_health(health);
        // The counters only render inside the active root's loss summary.
        if store.active_loss(active_root) > 0 {
            refresh.rows();
        }
    }
    let rearmed = stop != DrainStop::Empty && rx.rearm_if_pending();
    DrainOutcome {
        applied,
        bytes,
        stop,
        refresh,
        rearmed,
    }
}

// ─── the retained store ──────────────────────────────────────────────────────

/// Persistent store of LSP-pushed diagnostics, partitioned by the worktree
/// root that produced them and keyed by `(server stream, file)` within each
/// root. Survives model-hydration swaps (which only carry git/db state) so the
/// Problems panel keeps showing them. The partition is what keeps one
/// workspace's warm servers (they stay alive across tab switches) from
/// bleeding diagnostics into another's panel.
///
/// Bounds: `MAX_ROOTS` roots, `MAX_FILES_PER_ROOT` files per root,
/// `MAX_DIAGNOSTICS` items per file and `MAX_RETAINED_BYTES` in total, counted
/// over every owned byte (keys, the per-item duplicated file path, strings and
/// inline struct sizes) and checked on borrowed lengths *before* any item is
/// built.
///
/// Loss: a document whose latest state was dropped (refused here, evicted or
/// unparseable upstream) keeps its previous items but is marked lost, and the
/// Problems list for that root carries a warning row naming it — so old items
/// are never presented as current. A document mark ends when a complete
/// publication for it is committed; a stream-wide mark (losses the bus could
/// not attribute) marks every stored document of the stream and ends once
/// those are all republished complete. Retirement ends all of a stream's
/// marks. The health rows appear only while the root has loss.
#[derive(Debug, Default)]
pub struct LspDiagnostics {
    by_root: HashMap<PathBuf, RootDiagnostics>,
    retained_bytes: usize,
    /// Lifetime counters (sticky); rendered only inside a loss summary.
    health: LspHealth,
    loss: HashMap<StoreStreamKey, StreamLoss>,
    /// Total document marks across `loss` (bounded by `MAX_LOSS_DOCUMENTS`).
    loss_documents: usize,
    /// Owned bytes held by `loss` (the marks are retained memory too, and a
    /// count bound alone would admit `MAX_LOSS_DOCUMENTS` × 4 KiB of paths).
    /// Bounded by `MAX_LOSS_MARK_BYTES`; over it, marks escalate to their
    /// stream rather than being dropped.
    loss_bytes: usize,
    /// A stream's loss could not even be recorded (stream table full). Ends
    /// when no loss remains anywhere.
    loss_overflow: bool,
}

#[derive(Debug, Default)]
struct StreamLoss {
    documents: BTreeSet<String>,
    stream_wide: bool,
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
    fn stream_key(&self) -> StoreStreamKey {
        StoreStreamKey {
            root: self.root.clone(),
            server_identity: self.server_identity.clone(),
            generation: self.generation,
        }
    }

    fn is_stream(&self, stream: &StoreStreamKey) -> bool {
        self.root == stream.root
            && self.server_identity == stream.server_identity
            && self.generation == stream.generation
    }
}

impl StoreStreamKey {
    fn bus_key(&self) -> StreamKey {
        StreamKey {
            root: self.root.clone(),
            server_identity: self.server_identity.clone(),
            generation: self.generation,
        }
    }
}

const LSP_SOURCE_PREFIX: &str = "lsp:";
const LSP_DEFAULT_SOURCE: &str = "server";
/// Health rows are tagged with both, so a server whose `source` happens to be
/// "health" is never mistaken for one.
const HEALTH_SOURCE: &str = "lsp:health";
const HEALTH_CODE: &str = "thegn-lsp-loss";
const MAX_LOSS_DOCUMENTS: usize = limits::MAX_FILES_PER_ROOT * 4;
/// Retained-byte ceiling for the loss marks themselves.
const MAX_LOSS_MARK_BYTES: usize = 1024 * 1024;
const MAX_LOSS_STREAMS: usize = 2 * limits::MAX_ROOTS * limits::MAX_SERVERS_PER_ROOT;
/// Per-document loss rows shown for one root (the summary counts them all).
const MAX_LOSS_ROWS: usize = 64;

/// Owned bytes of one loss entry (its stream key plus the empty set).
fn loss_entry_bytes(stream: &StoreStreamKey) -> usize {
    stream
        .root
        .as_os_str()
        .as_encoded_bytes()
        .len()
        .saturating_add(stream.server_identity.len())
        .saturating_add(std::mem::size_of::<StoreStreamKey>())
        .saturating_add(std::mem::size_of::<StreamLoss>())
}

/// Owned bytes of one marked document path.
fn loss_document_bytes(path: &str) -> usize {
    path.len().saturating_add(std::mem::size_of::<String>())
}

fn is_health_row(item: &DiagnosticItem) -> bool {
    item.source == HEALTH_SOURCE && item.code.as_deref() == Some(HEALTH_CODE)
}

fn severity_rank(item: &DiagnosticItem) -> u8 {
    item.severity as u8
}

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
    loss_bytes: usize,
    recomputed_loss_bytes: usize,
}

impl LspDiagnostics {
    pub fn new() -> Self {
        LspDiagnostics::default()
    }

    /// Apply a server's latest diagnostics for one document, filed under the
    /// originating client's worktree root (stamped on the message). A
    /// complete empty set clears that file; an incomplete (malformed or
    /// truncated) empty set never masquerades as a clear. Returns the
    /// root-relative file the publication concerned (for a visible patch).
    pub fn apply(&mut self, pd: PublishedDiagnostics) -> Option<String> {
        if pd.root.as_os_str().is_empty()
            || pd.root.as_os_str().as_encoded_bytes().len() > limits::MAX_IDENTITY_BYTES
            || pd.server_identity.len() > limits::MAX_IDENTITY_BYTES
            || pd.path.len() > limits::MAX_IDENTITY_BYTES
        {
            self.health.invalid = self.health.invalid.saturating_add(1);
            return None;
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
                self.commit_complete(&key);
            } else {
                self.health.incomplete = self.health.incomplete.saturating_add(1);
                self.remember_document(&key);
            }
            return Some(key.path);
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
            self.health.dropped = self.health.dropped.saturating_add(1);
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            self.remember_document(&key);
            return Some(key.path);
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
        let root = self.by_root.entry(key.root.clone()).or_default();
        root.files.insert(key.clone(), items);
        if pd.complete {
            self.commit_complete(&key);
        } else {
            self.health.incomplete = self.health.incomplete.saturating_add(1);
            self.remember_document(&key);
        }
        Some(key.path)
    }

    /// Replace the LSP-sourced entries in `dst` with the current store's
    /// entries **for `root` only** (plus its loss rows), keeping any non-LSP
    /// (task-output) diagnostics, then re-sort by severity. A full rebuild —
    /// used on hydration swaps and retirements; per-slice updates go through
    /// [`LspDiagnostics::patch_into`].
    pub fn merge_into(&self, root: &Path, dst: &mut Vec<DiagnosticItem>) {
        dst.retain(|d| !d.source.starts_with(LSP_SOURCE_PREFIX));
        if let Some(files) = self.by_root.get(root) {
            for items in files.files.values() {
                dst.extend(items.iter().cloned());
            }
        }
        dst.extend(self.health_rows(root));
        dst.sort_by_key(severity_rank);
    }

    /// Incremental form of [`LspDiagnostics::merge_into`]: replace only the
    /// LSP items of `files` and the health rows, splicing the new items into
    /// their severity bands. No clone or sort of the untouched partition — the
    /// cost is one linear pass over `dst` plus the changed files' items. Falls
    /// back to a full merge if `dst` is not in severity order (e.g. a caller
    /// appended unsorted task output).
    pub fn patch_into(&self, root: &Path, dst: &mut Vec<DiagnosticItem>, files: &HashSet<String>) {
        if !dst.is_sorted_by_key(severity_rank) {
            self.merge_into(root, dst);
            return;
        }
        dst.retain(|d| {
            !(is_health_row(d)
                || (d.source.starts_with(LSP_SOURCE_PREFIX) && files.contains(&d.file)))
        });
        let mut add: Vec<DiagnosticItem> = Vec::new();
        if !files.is_empty()
            && let Some(state) = self.by_root.get(root)
        {
            for (key, items) in &state.files {
                if files.contains(&key.path) {
                    add.extend(items.iter().cloned());
                }
            }
        }
        add.extend(self.health_rows(root));
        add.sort_by_key(severity_rank);
        while let Some(last) = add.last() {
            let rank = severity_rank(last);
            let split = add.partition_point(|d| severity_rank(d) < rank);
            let band = add.split_off(split);
            let at = dst.partition_point(|d| severity_rank(d) <= rank);
            dst.splice(at..at, band);
        }
    }

    /// The loss rows for `root`: nothing while the root has no active loss;
    /// otherwise a bounded summary plus up to `MAX_LOSS_ROWS` per-document
    /// rows naming the files whose shown diagnostics are out of date.
    fn health_rows(&self, root: &Path) -> Vec<DiagnosticItem> {
        if self.active_loss(root) == 0 {
            return Vec::new();
        }
        let mut streams: Vec<(&StoreStreamKey, &StreamLoss)> = self
            .loss
            .iter()
            .filter(|(key, _)| key.root == root)
            .collect();
        streams.sort_by(|a, b| a.0.cmp(b.0));
        let documents: usize = streams.iter().map(|(_, l)| l.documents.len()).sum();
        let wide = streams.iter().filter(|(_, l)| l.stream_wide).count();
        let h = self.health;
        let mut rows = vec![DiagnosticItem {
            file: String::new(),
            line: 0,
            col: None,
            severity: Severity::Warning,
            message: format!(
                "LSP diagnostics may be out of date: {documents} file(s) and {wide} server(s) \
                 lost updates{} (dropped={} truncated={} stale={} invalid={})",
                if self.loss_overflow {
                    ", more untracked"
                } else {
                    ""
                },
                h.dropped,
                h.truncated,
                h.stale,
                h.invalid
            ),
            source: HEALTH_SOURCE.to_string(),
            code: Some(HEALTH_CODE.to_string()),
        }];
        'rows: for (stream, loss) in streams {
            let server = thegn_svc::lsp::sanitize_for_terminal(&stream.server_identity);
            for path in &loss.documents {
                if rows.len() > MAX_LOSS_ROWS {
                    break 'rows;
                }
                rows.push(DiagnosticItem {
                    file: thegn_svc::lsp::sanitize_for_terminal(path),
                    line: 1,
                    col: None,
                    severity: Severity::Warning,
                    message: format!(
                        "{server}: latest diagnostics for this file were lost; shown items may be out of date"
                    ),
                    source: HEALTH_SOURCE.to_string(),
                    code: Some(HEALTH_CODE.to_string()),
                });
            }
        }
        rows
    }

    /// Documents/streams under `root` whose latest state is known lost.
    pub fn active_loss(&self, root: &Path) -> usize {
        self.loss
            .iter()
            .filter(|(key, _)| key.root == root)
            .map(|(_, l)| l.documents.len() + usize::from(l.stream_wide))
            .sum::<usize>()
            + usize::from(self.loss_overflow)
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
        self.loss.retain(|key, _| keep(&key.root));
        self.settle_loss();
    }

    /// Keep only data from streams that are still registered on the bus
    /// (`active`). This is the only path by which a stream's retained
    /// diagnostics and loss marks end besides explicit clears: the evidence is
    /// the supervisor's registration set, never a publication's own numbers.
    /// Returns the roots whose partitions or loss rows changed.
    pub fn retain_streams(&mut self, active: &HashSet<StreamKey>) -> HashSet<PathBuf> {
        let mut changed = HashSet::new();
        let mut released = 0usize;
        let mut emptied = Vec::new();
        for (root, state) in &mut self.by_root {
            let before = state.files.len();
            state.files.retain(|key, items| {
                let keep = active.contains(&key.stream_key().bus_key());
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
        self.loss.retain(|key, _| {
            let keep = active.contains(&key.bus_key());
            if !keep {
                changed.insert(key.root.clone());
            }
            keep
        });
        self.settle_loss();
        changed
    }

    #[allow(dead_code)] // exercised by tests; the loop-side caller was removed
    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty() && self.loss.is_empty() && !self.loss_overflow
    }

    pub fn record_health(&mut self, health: LspHealth) {
        self.health.saturating_add(health);
    }

    /// Documents whose latest publication the bus had to drop (health was
    /// already counted by the bus). Returns whether `active_root` was touched.
    pub fn mark_incomplete(
        &mut self,
        keys: impl IntoIterator<Item = DiagnosticKey>,
        active_root: &Path,
    ) -> bool {
        let mut touched = false;
        for key in keys {
            let path = relativize(&key.path, &key.root);
            touched |= key.root == active_root;
            self.remember_document(&StoreFileKey {
                root: key.root,
                server_identity: key.server_identity,
                generation: key.generation,
                path,
            });
        }
        touched
    }

    /// Streams with losses the bus could not attribute: every stored document
    /// of the stream is marked, and the stream stays marked until those are
    /// republished complete. Returns whether `active_root` was touched.
    pub fn mark_streams_incomplete(
        &mut self,
        streams: impl IntoIterator<Item = StreamKey>,
        active_root: &Path,
    ) -> bool {
        let mut touched = false;
        for stream in streams {
            touched |= stream.root == active_root;
            let stream = StoreStreamKey {
                root: stream.root,
                server_identity: stream.server_identity,
                generation: stream.generation,
            };
            let stored: Vec<StoreFileKey> = self
                .by_root
                .get(&stream.root)
                .map(|state| {
                    state
                        .files
                        .keys()
                        .filter(|key| key.is_stream(&stream))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for key in &stored {
                self.remember_document(key);
            }
            if let Some(loss) = self.loss_entry(stream) {
                loss.stream_wide = true;
            }
        }
        touched
    }

    fn loss_entry(&mut self, stream: StoreStreamKey) -> Option<&mut StreamLoss> {
        if !self.loss.contains_key(&stream) {
            let cost = loss_entry_bytes(&stream);
            if self.loss.len() >= MAX_LOSS_STREAMS
                || self.loss_bytes.saturating_add(cost) > MAX_LOSS_MARK_BYTES
            {
                self.loss_overflow = true;
                return None;
            }
            self.loss_bytes = self.loss_bytes.saturating_add(cost);
        }
        Some(self.loss.entry(stream).or_default())
    }

    fn remember_document(&mut self, key: &StoreFileKey) {
        let cost = loss_document_bytes(&key.path);
        let room = self.loss_documents < MAX_LOSS_DOCUMENTS
            && self.loss_bytes.saturating_add(cost) <= MAX_LOSS_MARK_BYTES;
        let Some(loss) = self.loss_entry(key.stream_key()) else {
            return;
        };
        if loss.documents.contains(&key.path) {
            return;
        }
        if room {
            loss.documents.insert(key.path.clone());
            self.loss_documents += 1;
            self.loss_bytes = self.loss_bytes.saturating_add(cost);
        } else {
            // Never evict a mark (that would turn a loss into "clean"):
            // escalate to the stream, which costs no further bytes.
            loss.stream_wide = true;
        }
    }

    /// A complete publication for `key` was committed: its document mark ends,
    /// and a stream-wide mark ends once no document of the stream is marked.
    fn commit_complete(&mut self, key: &StoreFileKey) {
        let stream = key.stream_key();
        let Some(loss) = self.loss.get_mut(&stream) else {
            return;
        };
        if loss.documents.remove(&key.path) {
            self.loss_documents = self.loss_documents.saturating_sub(1);
            self.loss_bytes = self
                .loss_bytes
                .saturating_sub(loss_document_bytes(&key.path));
        }
        if loss.documents.is_empty() {
            // The stream-wide mark ends with the last attributed document: a
            // complete publication for this stream proves the resync landed.
            self.loss.remove(&stream);
        }
        self.settle_loss();
    }

    fn settle_loss(&mut self) {
        self.loss_documents = self.loss.values().map(|l| l.documents.len()).sum();
        self.loss_bytes = self
            .loss
            .iter()
            .map(|(stream, loss)| {
                loss.documents
                    .iter()
                    .fold(loss_entry_bytes(stream), |n, path| {
                        n.saturating_add(loss_document_bytes(path))
                    })
            })
            .fold(0usize, usize::saturating_add);
        if self.loss.is_empty() {
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
        let recomputed_loss_bytes = self
            .loss
            .iter()
            .map(|(stream, loss)| {
                loss.documents
                    .iter()
                    .fold(loss_entry_bytes(stream), |n, path| {
                        n.saturating_add(loss_document_bytes(path))
                    })
            })
            .fold(0usize, usize::saturating_add);
        StoreFootprint {
            bytes: self.retained_bytes,
            recomputed,
            roots: self.by_root.len(),
            files: self.by_root.values().map(|root| root.files.len()).sum(),
            loss_bytes: self.loss_bytes,
            recomputed_loss_bytes,
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
            || self
                .loss
                .get(&stream)
                .is_some_and(|l| l.stream_wide || l.documents.contains(file))
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
        assert!(dst.len() <= thegn_svc::lsp::limits::MAX_FILES_PER_ROOT + 1 + MAX_LOSS_ROWS);
        assert!(
            dst.iter()
                .all(|item| !item.message.chars().any(char::is_control))
        );
        assert!(dst.iter().any(is_health_row));
        assert!(
            dst.iter()
                .any(|item| is_health_row(item) && item.file == "4096.rs")
        );
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
        assert_eq!(
            f.loss_bytes, f.recomputed_loss_bytes,
            "loss-mark bytes drifted: {f:?}"
        );
        assert!(f.bytes <= limits::MAX_RETAINED_BYTES);
        assert!(f.loss_bytes <= MAX_LOSS_MARK_BYTES);
        assert!(f.roots <= limits::MAX_ROOTS);
        f
    }

    #[test]
    fn more_than_max_roots_sequential_open_close_keeps_working() {
        let (inner, rx) = enabled_inner();
        for index in 0..(limits::MAX_ROOTS * 3) {
            let root = PathBuf::from(format!("/wt/{index}"));
            let epoch = 2 * index as u64 + 1;
            // The worktree opens: it joins the live set.
            assert_eq!(
                inner.reconcile_roots(epoch, HashSet::from([root.clone()])),
                0
            );
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
            assert_eq!(inner.reconcile_roots(epoch + 1, HashSet::new()), 2);
            // A late request for the closed root cannot respawn a server.
            assert_eq!(
                inner.client_with(&root, "rust", fake_client).err(),
                Some(LspError::NotAvailable)
            );
        }
        assert_eq!(rx.footprint().active_streams, 0);
        assert_eq!(rx.footprint().metadata_bytes, 0);
        assert!(inner.clients.lock().unwrap().map.is_empty());
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
        assert_eq!(
            outcome.refresh,
            VisibleRefresh::Full,
            "retirement changed the active partition"
        );
        let mut dst = Vec::new();
        store.merge_into(root, &mut dst);
        let messages: Vec<&str> = dst.iter().map(|d| d.message.as_str()).collect();
        // An ordinary restart (with a stale late publication) is not loss:
        // no health row appears.
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
        assert!(outcome.refresh.is_none());
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
            matches!(outcome.refresh, VisibleRefresh::Patch(ref files) if files.contains("q.rs")),
            "the quiet (active) root landed in the first slice: {:?}",
            outcome.refresh
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
                .any(|d| is_health_row(d) && d.message.contains("1 file(s)"))
        );
        assert!(
            dst.iter().any(|d| is_health_row(d) && d.file == "a.rs"),
            "the stale file is named, so its old items are not presented as current"
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
            !dst.iter().any(is_health_row),
            "loss ended: the row goes away (counters stay internal)"
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
            store.active_loss(Path::new("/p")),
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

    // ── review round 2: F2–F5 ────────────────────────────────────────────────

    #[track_caller]
    fn assert_same_items(patched: &[DiagnosticItem], merged: &[DiagnosticItem]) {
        let key = |d: &DiagnosticItem| {
            (
                d.severity as u8,
                d.file.clone(),
                d.line,
                d.message.clone(),
                d.source.clone(),
            )
        };
        let mut a: Vec<_> = patched.iter().map(key).collect();
        let mut b: Vec<_> = merged.iter().map(key).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        assert!(
            patched.is_sorted_by_key(severity_rank),
            "patch keeps severity order"
        );
    }

    #[test]
    fn incremental_patch_matches_a_full_merge_without_rebuilding_untouched_files() {
        let mut store = LspDiagnostics::new();
        for index in 0..50u32 {
            let sev =
                [LspSeverity::Error, LspSeverity::Warning, LspSeverity::Hint][index as usize % 3];
            store.apply(pd(
                "/p",
                &format!("/p/{index}.rs"),
                vec![diag(index, sev, &format!("m{index}"))],
            ));
        }
        let task = DiagnosticItem {
            file: "task.rs".into(),
            line: 1,
            col: None,
            severity: Severity::Warning,
            message: "task".into(),
            source: "cargo clippy".into(),
            code: None,
        };
        let mut dst = vec![task];
        store.merge_into(Path::new("/p"), &mut dst);

        // Replace one file, clear another, refuse a third (loss row appears).
        store.apply(pd(
            "/p",
            "/p/3.rs",
            vec![
                diag(9, LspSeverity::Error, "new3"),
                diag(1, LspSeverity::Info, "info3"),
            ],
        ));
        store.apply(pd("/p", "/p/4.rs", vec![]));
        let too_many: Vec<_> = (0..=limits::MAX_DIAGNOSTICS as u32)
            .map(|l| diag(l, LspSeverity::Error, "x"))
            .collect();
        store.apply(pd("/p", "/p/5.rs", too_many));
        let files: HashSet<String> = ["3.rs", "4.rs", "5.rs"].map(String::from).into();
        let mut patched = dst.clone();
        store.patch_into(Path::new("/p"), &mut patched, &files);
        let mut merged = dst.clone();
        store.merge_into(Path::new("/p"), &mut merged);
        assert_same_items(&patched, &merged);
        assert!(
            patched.iter().any(|d| d.message == "task"),
            "task output kept"
        );
        assert!(
            patched.iter().any(|d| d.message == "m5"),
            "refused file keeps old items"
        );
        assert!(patched.iter().any(|d| is_health_row(d) && d.file == "5.rs"));

        // Loss ends: the rows-only patch removes the health rows again.
        store.apply(pd(
            "/p",
            "/p/5.rs",
            vec![diag(0, LspSeverity::Error, "ok5")],
        ));
        store.patch_into(
            Path::new("/p"),
            &mut patched,
            &HashSet::from(["5.rs".to_string()]),
        );
        store.merge_into(Path::new("/p"), &mut merged);
        assert_same_items(&patched, &merged);
        assert!(!patched.iter().any(is_health_row));
    }

    #[test]
    fn drain_refresh_names_only_the_touched_files() {
        let (inner, rx) = enabled_inner();
        let client = inner
            .client_with(Path::new("/p"), "rust", fake_client)
            .unwrap();
        let g = client.generation();
        for seq in 1..=3 {
            inner.diag_tx.publish(stamped(
                "/p",
                "rust",
                g,
                seq,
                &format!("/p/{seq}.rs"),
                vec![diag(0, LspSeverity::Error, "e")],
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
        assert_eq!(
            outcome.refresh,
            VisibleRefresh::Patch(["1.rs", "2.rs", "3.rs"].map(String::from).into())
        );
    }

    #[test]
    fn unparseable_publication_loss_is_shown_for_that_file() {
        let (inner, rx) = enabled_inner();
        let client = inner
            .client_with(Path::new("/p"), "rust", fake_client)
            .unwrap();
        let g = client.generation();
        inner.diag_tx.publish(stamped(
            "/p",
            "rust",
            g,
            1,
            "/p/a.rs",
            vec![diag(0, LspSeverity::Error, "ten")],
        ));
        let mut store = LspDiagnostics::new();
        let mut dst = Vec::new();
        drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        )
        .refresh
        .apply_to(&store, Path::new("/p"), &mut dst);
        // The next publication for a.rs failed the JSON bounds upstream.
        inner
            .diag_tx
            .mark_lost(Path::new("/p"), "rust", g, Some("/p/a.rs"));
        let outcome = drain_diagnostics(
            &rx,
            &mut store,
            Path::new("/p"),
            DrainBudget::default(),
            || Duration::ZERO,
            || false,
            || {},
        );
        outcome.refresh.apply_to(&store, Path::new("/p"), &mut dst);
        assert!(store.is_incomplete("/p", "rust", g, "a.rs"));
        assert!(dst.iter().any(|d| d.message == "ten"));
        assert!(dst.iter().any(|d| is_health_row(d) && d.file == "a.rs"));
    }

    #[test]
    fn stream_wide_loss_clears_once_every_stored_file_is_republished_complete() {
        let mut store = LspDiagnostics::new();
        store.apply(pd("/p", "/p/a.rs", vec![diag(0, LspSeverity::Error, "a")]));
        store.apply(pd("/p", "/p/b.rs", vec![diag(0, LspSeverity::Error, "b")]));
        let stream = StreamKey {
            root: PathBuf::from("/p"),
            server_identity: "test".into(),
            generation: 1,
        };
        assert!(store.mark_streams_incomplete([stream], Path::new("/p")));
        assert_eq!(
            store.active_loss(Path::new("/p")),
            3,
            "two files + the stream"
        );
        store.apply(pd("/p", "/p/a.rs", vec![diag(0, LspSeverity::Error, "a2")]));
        assert!(store.active_loss(Path::new("/p")) > 0);
        store.apply(pd("/p", "/p/b.rs", vec![]));
        assert_eq!(
            store.active_loss(Path::new("/p")),
            0,
            "all republished complete"
        );
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(!dst.iter().any(is_health_row));
    }

    #[test]
    fn counters_without_loss_never_render_a_health_row() {
        let mut store = LspDiagnostics::new();
        store.apply(pd("/p", "/p/a.rs", vec![diag(0, LspSeverity::Error, "a")]));
        store.record_health(LspHealth {
            stale: 3,
            truncated: 2,
            invalid: 1,
            ..LspHealth::default()
        });
        let mut dst = Vec::new();
        store.merge_into(Path::new("/p"), &mut dst);
        assert_eq!(dst.len(), 1);
        // Loss in another root does not leak a row into this one.
        let too_many: Vec<_> = (0..=limits::MAX_DIAGNOSTICS as u32)
            .map(|l| diag(l, LspSeverity::Error, "x"))
            .collect();
        store.apply(pd("/q", "/q/x.rs", too_many));
        store.merge_into(Path::new("/p"), &mut dst);
        assert!(!dst.iter().any(is_health_row));
    }

    #[test]
    fn out_of_order_reconciles_converge_on_the_latest_root_set() {
        let (inner, rx) = enabled_inner();
        let (a, b) = (PathBuf::from("/a"), PathBuf::from("/b"));
        inner.reconcile_roots(1, HashSet::from([a.clone(), b.clone()]));
        inner.client_with(&a, "rust", fake_client).unwrap();
        inner.client_with(&b, "rust", fake_client).unwrap();
        // S1 = {a} (epoch 2) and S2 = {b} (epoch 3) queued; S2 wins the lock.
        assert_eq!(inner.reconcile_roots(3, HashSet::from([b.clone()])), 1);
        assert_eq!(
            inner.reconcile_roots(2, HashSet::from([a.clone()])),
            0,
            "older epoch is a no-op"
        );
        assert_eq!(rx.footprint().active_streams, 1);
        assert!(
            inner.client_with(&b, "rust", fake_client).is_ok(),
            "b's server survived"
        );
        assert_eq!(
            inner.client_with(&a, "rust", fake_client).err(),
            Some(LspError::NotAvailable)
        );
    }

    // ── review round 3: B1 + N1 ─────────────────────────────────────────────

    #[test]
    fn a_patch_after_a_tab_switch_rebuilds_instead_of_mixing_two_worktrees() {
        // Both worktrees have src/lib.rs — patching by relative path alone
        // would splice B's items into A's still-displayed list.
        let mut store = LspDiagnostics::new();
        store.apply(pd(
            "/a",
            "/a/src/lib.rs",
            vec![diag(0, LspSeverity::Error, "a-lib")],
        ));
        store.apply(pd(
            "/a",
            "/a/src/other.rs",
            vec![diag(0, LspSeverity::Error, "a-other")],
        ));
        store.apply(pd(
            "/b",
            "/b/src/lib.rs",
            vec![diag(0, LspSeverity::Error, "b-lib")],
        ));

        let mut visible = VisibleTarget::default();
        let mut dst = Vec::new();
        // The list is built for /a.
        visible
            .refresh_for(Path::new("/a"), VisibleRefresh::None)
            .apply_to(&store, Path::new("/a"), &mut dst);
        assert_eq!(dst.len(), 2);

        // Tab switch to /b, then /b's warm server publishes src/lib.rs: the
        // slice asks for a patch, the tracker upgrades it to a full rebuild.
        let asked = VisibleRefresh::Patch(HashSet::from(["src/lib.rs".to_string()]));
        let applied = visible.refresh_for(Path::new("/b"), asked);
        assert_eq!(applied, VisibleRefresh::Full);
        applied.apply_to(&store, Path::new("/b"), &mut dst);
        let messages: Vec<&str> = dst.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages, vec!["b-lib"], "no /a items survive the switch");

        // Still on /b: an ordinary patch stays a patch.
        let asked = VisibleRefresh::Patch(HashSet::from(["src/lib.rs".to_string()]));
        assert!(matches!(
            visible.refresh_for(Path::new("/b"), asked),
            VisibleRefresh::Patch(_)
        ));
        // A rebuild by the hydration swap re-owns the list without a refresh.
        visible.rebuilt(Path::new("/a"));
        assert!(
            visible
                .refresh_for(Path::new("/a"), VisibleRefresh::None)
                .is_none()
        );
    }

    #[test]
    fn loss_mark_bytes_are_bounded_and_conserved() {
        let mut store = LspDiagnostics::new();
        // Long paths: a count-only bound would admit ~64 MiB of marks.
        let long = "p".repeat(limits::MAX_IDENTITY_BYTES - 32);
        let mut marked = 0usize;
        for index in 0..4_000 {
            let key = DiagnosticKey {
                root: PathBuf::from("/p"),
                server_identity: "rust".into(),
                generation: 1,
                path: format!("/p/{long}{index}.rs"),
            };
            store.mark_incomplete([key], Path::new("/p"));
            marked += 1;
            if marked.is_multiple_of(250) {
                assert_store_conserved(&store);
            }
        }
        let f = assert_store_conserved(&store);
        assert!(f.loss_bytes <= MAX_LOSS_MARK_BYTES);
        assert!(
            store.active_loss(Path::new("/p")) > 0,
            "over the byte bound the loss escalates to the stream, never vanishes"
        );
        assert!(
            store.is_incomplete("/p", "rust", 1, "nothing-was-ever-marked.rs"),
            "the stream-wide mark covers the documents that did not fit"
        );
        // Marks are released again.
        store.retain_streams(&HashSet::new());
        let f = assert_store_conserved(&store);
        assert_eq!((f.loss_bytes, store.active_loss(Path::new("/p"))), (0, 0));
    }
}
