//! The bounded, latest-per-document diagnostics bus between language-server
//! reader threads and the host event loop.
//!
//! One mutex guards everything — the authority registry, the pending queue,
//! loss marks, health and the wake obligation — so every admission decision,
//! byte adjustment and wake transition is atomic with respect to the others.
//!
//! **Authority.** A stream is `(root, server_identity, generation)`. The
//! generation is minted here, by [`DiagnosticsSender::register`], when the
//! supervisor starts a client; a publication is admitted only while its exact
//! generation is the registered one. Retiring a stream removes its registry
//! entry (releasing capacity) and purges its queued publications. Generations
//! are never reused, so a retired generation stays stale forever without any
//! tombstone history — there is nothing to evict and nothing to replay.
//!
//! **Queue.** Pending publications are keyed per document within a stream and
//! replaced in place (latest wins; a clear replaces an older update). Ready
//! roots are served round-robin so a root flooding unique documents cannot
//! starve a quiet one. Over capacity, the oldest pending document of the root
//! with the most pending documents is evicted (deterministically), and the
//! loss is recorded as an incomplete mark rather than silently vanishing.
//!
//! **Accounting.** `bytes` is maintained incrementally and is exactly the sum
//! of the per-entry costs below (including the owned key clones held by the map
//! and the ready order) plus a per-ready-root cost. `metadata_bytes` is exactly
//! the registry plus document-loss-mark footprint. Both are checked before
//! anything is retained; [`DiagnosticsReceiver::footprint`] recomputes both
//! from scratch so tests can assert conservation after every operation.
//!
//! **Wake.** `wake_pending` is a single bounded obligation. Producers set it
//! (and notify) on any state the host must observe; the bridge thread's
//! [`DiagnosticsReceiver::wait`] consumes it. The host re-arms it only when it
//! deliberately stopped with work still queued. An idle queue therefore blocks
//! the bridge on the condvar: zero wakes, zero polling.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::{LspDiagnostic, LspError, LspHealth, PublishedDiagnostics, limits};

/// One document within one authority stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiagnosticKey {
    pub root: PathBuf,
    pub server_identity: String,
    pub generation: u64,
    pub path: String,
}

/// One authority stream: a root, a server identity and its minted generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamKey {
    pub root: PathBuf,
    pub server_identity: String,
    pub generation: u64,
}

/// Lost-state marks handed to the host: documents whose latest publication
/// was dropped, and whole streams whose losses could not be tracked per
/// document (the mark set was full). Stream marks end only with the stream.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LossMarks {
    pub documents: Vec<DiagnosticKey>,
    pub streams: Vec<StreamKey>,
}

impl LossMarks {
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty() && self.streams.is_empty()
    }
}

/// The retained footprint, maintained vs recomputed — test/diagnostic only.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFootprint {
    pub bytes: usize,
    pub recomputed_bytes: usize,
    pub metadata_bytes: usize,
    pub recomputed_metadata_bytes: usize,
    pub documents: usize,
    pub active_streams: usize,
}

#[derive(Debug)]
struct StreamState {
    generation: u64,
    last_sequence: u64,
    /// A loss that could not be recorded per document.
    lossy: bool,
}

#[derive(Debug)]
struct Entry {
    publication: PublishedDiagnostics,
    cost: usize,
}

#[derive(Debug)]
struct QueueState {
    /// root → server identity → current stream.
    active: HashMap<PathBuf, HashMap<String, StreamState>>,
    next_generation: u64,
    pending: HashMap<DiagnosticKey, Entry>,
    /// Per-root FIFO of pending documents (first-admitted first).
    per_root: HashMap<PathBuf, VecDeque<DiagnosticKey>>,
    /// Roots with pending documents, served round-robin.
    ready: VecDeque<PathBuf>,
    bytes: usize,
    metadata_bytes: usize,
    loss_marks: HashSet<DiagnosticKey>,
    health: LspHealth,
    /// A retirement the host has not yet reconciled its store against.
    retired_dirty: bool,
    wake_pending: bool,
    closed: bool,
}

impl Default for QueueState {
    fn default() -> Self {
        QueueState {
            active: HashMap::new(),
            next_generation: 1,
            pending: HashMap::new(),
            per_root: HashMap::new(),
            ready: VecDeque::new(),
            bytes: 0,
            metadata_bytes: 0,
            loss_marks: HashSet::new(),
            health: LspHealth::default(),
            retired_dirty: false,
            wake_pending: false,
            closed: false,
        }
    }
}

// ─── cost model ──────────────────────────────────────────────────────────────

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

/// Every owned byte one publication retains: its strings, its diagnostics and
/// their inline struct sizes. Shared with the host drain budget so both sides
/// account one publication identically.
pub fn published_diagnostics_bytes(pd: &PublishedDiagnostics) -> usize {
    let diagnostic_bytes = pd.diagnostics.iter().fold(0usize, |n, d| {
        n.saturating_add(d.message.len())
            .saturating_add(d.code.as_ref().map_or(0, String::len))
            .saturating_add(d.source.as_ref().map_or(0, String::len))
            .saturating_add(std::mem::size_of::<LspDiagnostic>())
    });
    path_bytes(&pd.root)
        .saturating_add(pd.path.len())
        .saturating_add(pd.server_identity.len())
        .saturating_add(diagnostic_bytes)
        .saturating_add(std::mem::size_of::<PublishedDiagnostics>())
}

/// One owned `DiagnosticKey` clone.
pub fn diagnostic_key_bytes(key: &DiagnosticKey) -> usize {
    path_bytes(&key.root)
        .saturating_add(key.server_identity.len())
        .saturating_add(key.path.len())
        .saturating_add(std::mem::size_of::<DiagnosticKey>())
}

fn key_bytes_of(pd: &PublishedDiagnostics) -> usize {
    path_bytes(&pd.root)
        .saturating_add(pd.server_identity.len())
        .saturating_add(pd.path.len())
        .saturating_add(std::mem::size_of::<DiagnosticKey>())
}

/// A pending entry: the publication, the map key and the per-root order key.
fn entry_cost(pd: &PublishedDiagnostics) -> usize {
    published_diagnostics_bytes(pd)
        .saturating_add(key_bytes_of(pd).saturating_mul(2))
        .saturating_add(std::mem::size_of::<Entry>())
}

/// A root present in `per_root` and `ready`: two owned path clones.
fn ready_root_cost(root: &Path) -> usize {
    path_bytes(root)
        .saturating_mul(2)
        .saturating_add(std::mem::size_of::<PathBuf>() * 2)
        .saturating_add(std::mem::size_of::<VecDeque<DiagnosticKey>>())
}

fn stream_cost(root: &Path, identity: &str, first_on_root: bool) -> usize {
    let root_cost = if first_on_root {
        path_bytes(root)
            .saturating_add(std::mem::size_of::<PathBuf>())
            .saturating_add(std::mem::size_of::<HashMap<String, StreamState>>())
    } else {
        0
    };
    root_cost
        .saturating_add(identity.len())
        .saturating_add(std::mem::size_of::<String>())
        .saturating_add(std::mem::size_of::<StreamState>())
}

impl QueueState {
    fn stream(&self, root: &Path, identity: &str) -> Option<&StreamState> {
        self.active.get(root).and_then(|m| m.get(identity))
    }

    fn stream_mut(&mut self, root: &Path, identity: &str) -> Option<&mut StreamState> {
        self.active.get_mut(root).and_then(|m| m.get_mut(identity))
    }

    fn is_current(&self, root: &Path, identity: &str, generation: u64) -> bool {
        self.stream(root, identity)
            .is_some_and(|s| s.generation == generation)
    }

    fn signal(&mut self, notify: &Condvar) {
        self.wake_pending = true;
        notify.notify_all();
    }

    /// Remove one pending document everywhere it is referenced, returning the
    /// publication. Keeps `bytes`, `per_root` and `ready` consistent.
    fn remove_pending(&mut self, key: &DiagnosticKey) -> Option<PublishedDiagnostics> {
        let entry = self.pending.remove(key)?;
        self.bytes = self.bytes.saturating_sub(entry.cost);
        let emptied = if let Some(queue) = self.per_root.get_mut(&key.root) {
            if let Some(index) = queue.iter().position(|candidate| candidate == key) {
                queue.remove(index); // the order clone; its cost is in `entry.cost`
            }
            queue.is_empty()
        } else {
            false
        };
        if emptied {
            self.drop_ready_root(&key.root);
        }
        Some(entry.publication)
    }

    fn drop_ready_root(&mut self, root: &Path) {
        if self.per_root.remove(root).is_some() {
            self.bytes = self.bytes.saturating_sub(ready_root_cost(root));
            self.ready.retain(|candidate| candidate != root);
        }
    }

    /// Record that `key`'s latest publication was lost. Falls back to a
    /// stream-level mark when the document mark set is at capacity.
    fn mark_loss(&mut self, key: DiagnosticKey) {
        self.health.incomplete = self.health.incomplete.saturating_add(1);
        if self.loss_marks.contains(&key) {
            return;
        }
        let cost = diagnostic_key_bytes(&key);
        if self.loss_marks.len() < limits::MAX_LOSS_MARKS
            && self.metadata_bytes.saturating_add(cost) <= limits::MAX_QUEUE_METADATA_BYTES
        {
            self.metadata_bytes = self.metadata_bytes.saturating_add(cost);
            self.loss_marks.insert(key);
        } else if let Some(stream) = self.stream_mut(&key.root, &key.server_identity)
            && stream.generation == key.generation
        {
            stream.lossy = true;
        }
    }

    fn clear_loss(&mut self, key: &DiagnosticKey) {
        if self.loss_marks.remove(key) {
            self.metadata_bytes = self
                .metadata_bytes
                .saturating_sub(diagnostic_key_bytes(key));
        }
    }

    /// Evict the oldest pending document of the root holding the most pending
    /// documents. Ties prefer the incoming root, then the smallest root path —
    /// deterministic, and a quiet root is never evicted for a flooding one.
    fn evict_one(&mut self, incoming_root: &Path) -> bool {
        let victim_root = self
            .per_root
            .iter()
            .max_by(|(a_root, a), (b_root, b)| {
                a.len()
                    .cmp(&b.len())
                    .then_with(|| {
                        (a_root.as_path() == incoming_root)
                            .cmp(&(b_root.as_path() == incoming_root))
                    })
                    .then_with(|| b_root.cmp(a_root))
            })
            .map(|(root, _)| root.clone());
        let Some(victim) = victim_root
            .and_then(|root| self.per_root.get(&root))
            .and_then(|queue| queue.front().cloned())
        else {
            return false;
        };
        self.remove_pending(&victim); // the loss is recorded below
        self.health.dropped = self.health.dropped.saturating_add(1);
        self.mark_loss(victim);
        true
    }

    fn recompute(&self) -> (usize, usize) {
        let pending = self
            .pending
            .values()
            .map(|entry| entry_cost(&entry.publication))
            .fold(0usize, usize::saturating_add);
        let roots = self
            .per_root
            .keys()
            .map(|root| ready_root_cost(root))
            .fold(0usize, usize::saturating_add);
        let registry = self
            .active
            .iter()
            .map(|(root, streams)| {
                streams
                    .keys()
                    .enumerate()
                    .map(|(index, identity)| stream_cost(root, identity, index == 0))
                    .fold(0usize, usize::saturating_add)
            })
            .fold(0usize, usize::saturating_add);
        let marks = self
            .loss_marks
            .iter()
            .map(diagnostic_key_bytes)
            .fold(0usize, usize::saturating_add);
        (
            pending.saturating_add(roots),
            registry.saturating_add(marks),
        )
    }
}

// ─── handles ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Shared {
    state: Mutex<QueueState>,
    notify: Condvar,
}

impl Shared {
    fn lock(&self) -> Option<MutexGuard<'_, QueueState>> {
        self.state.lock().ok()
    }
}

/// Producer half, cloned into the supervisor and every client reader. Never
/// blocks on the consumer: admission is O(pending) under a short lock.
#[derive(Clone, Debug)]
pub struct DiagnosticsSender {
    shared: Arc<Shared>,
}

/// Consumer half: the host loop drains it; the bridge thread waits on it.
#[derive(Clone, Debug)]
pub struct DiagnosticsReceiver {
    shared: Arc<Shared>,
}

pub fn diagnostics_channel() -> (DiagnosticsSender, DiagnosticsReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(QueueState::default()),
        notify: Condvar::new(),
    });
    (
        DiagnosticsSender {
            shared: shared.clone(),
        },
        DiagnosticsReceiver { shared },
    )
}

impl DiagnosticsSender {
    /// Register a new authority stream for `(root, identity)` and mint its
    /// generation. Any previous stream for the pair is retired first (its
    /// queued publications purged), so a restart never inherits old data.
    /// Sizes are validated on the borrowed inputs before anything is cloned.
    pub fn register(&self, root: &Path, identity: &str) -> Result<u64, LspError> {
        if path_bytes(root) == 0
            || path_bytes(root) > limits::MAX_IDENTITY_BYTES
            || identity.is_empty()
            || identity.len() > limits::MAX_IDENTITY_BYTES
        {
            return Err(LspError::Bounded("LSP authority identity limit".into()));
        }
        let Some(mut state) = self.shared.lock() else {
            return Err(LspError::Protocol("diagnostics bus poisoned".into()));
        };
        if state.closed {
            return Err(LspError::NotAvailable);
        }
        if let Some(previous) = state.stream(root, identity).map(|s| s.generation) {
            retire_locked(&mut state, root, identity, previous);
        }
        let root_known = state.active.contains_key(root);
        if !root_known && state.active.len() >= limits::MAX_ROOTS {
            return Err(LspError::Bounded("LSP root limit reached".into()));
        }
        if state
            .active
            .get(root)
            .is_some_and(|streams| streams.len() >= limits::MAX_SERVERS_PER_ROOT)
        {
            return Err(LspError::Bounded(
                "LSP servers-per-root limit reached".into(),
            ));
        }
        let cost = stream_cost(root, identity, !root_known);
        if state.metadata_bytes.saturating_add(cost) > limits::MAX_QUEUE_METADATA_BYTES {
            return Err(LspError::Bounded(
                "LSP authority metadata limit reached".into(),
            ));
        }
        let generation = state.next_generation;
        if generation == 0 || generation == u64::MAX {
            return Err(LspError::Bounded("LSP generation space exhausted".into()));
        }
        state.next_generation = generation + 1;
        state.metadata_bytes = state.metadata_bytes.saturating_add(cost);
        state.active.entry(root.to_path_buf()).or_default().insert(
            identity.to_string(),
            StreamState {
                generation,
                last_sequence: 0,
                lossy: false,
            },
        );
        Ok(generation)
    }

    /// Retire exactly `generation` of `(root, identity)`. A late close of an
    /// older generation never retires a newer registration for the same pair.
    /// Returns whether a stream was retired.
    pub fn retire(&self, root: &Path, identity: &str, generation: u64) -> bool {
        let Some(mut state) = self.shared.lock() else {
            return false;
        };
        let retired = retire_locked(&mut state, root, identity, generation);
        if retired {
            state.signal(&self.shared.notify);
        }
        retired
    }

    /// Whether `(root, identity, generation)` is the registered stream. Reader
    /// threads check this before projecting a notification at all.
    pub fn is_current(&self, root: &Path, identity: &str, generation: u64) -> bool {
        self.shared
            .lock()
            .is_some_and(|state| state.is_current(root, identity, generation))
    }

    /// Record health findings (no-op for an empty report, so routine traffic
    /// never manufactures wakes).
    pub fn record(&self, health: LspHealth) {
        if !health.has_findings() {
            return;
        }
        if let Some(mut state) = self.shared.lock() {
            state.health.saturating_add(health);
            state.signal(&self.shared.notify);
        }
    }

    /// Admit one publication (latest-per-document). Authority and sequence are
    /// checked before any key is cloned; capacity is checked before anything
    /// is retained; every drop is health plus a loss mark.
    pub fn publish(&self, pd: PublishedDiagnostics) {
        let Some(mut state) = self.shared.lock() else {
            return;
        };
        let notify = &self.shared.notify;
        let Some(stream) = state
            .stream_mut(&pd.root, &pd.server_identity)
            .filter(|stream| stream.generation == pd.generation)
        else {
            state.health.stale = state.health.stale.saturating_add(1);
            state.signal(notify);
            return;
        };
        if pd.sequence <= stream.last_sequence {
            state.health.stale = state.health.stale.saturating_add(1);
            state.signal(notify);
            return;
        }
        stream.last_sequence = pd.sequence;

        let cost = entry_cost(&pd);
        let key = DiagnosticKey {
            root: pd.root.clone(),
            server_identity: pd.server_identity.clone(),
            generation: pd.generation,
            path: pd.path.clone(),
        };

        // Replacement: the pending value is superseded either way.
        if let Some(old_cost) = state.pending.get(&key).map(|entry| entry.cost) {
            if state.bytes.saturating_sub(old_cost).saturating_add(cost) > limits::MAX_QUEUE_BYTES {
                // Delivering the superseded value would show stale data as
                // current; drop it too and mark the document lost.
                state.remove_pending(&key); // superseded; loss recorded below
                state.health.dropped = state.health.dropped.saturating_add(1);
                state.mark_loss(key);
            } else {
                state.bytes = state.bytes.saturating_sub(old_cost).saturating_add(cost);
                state.clear_loss(&key);
                if let Some(entry) = state.pending.get_mut(&key) {
                    *entry = Entry {
                        publication: pd,
                        cost,
                    };
                }
            }
            state.signal(notify);
            return;
        }

        // New admission.
        let root_cost = if state.per_root.contains_key(&key.root) {
            0
        } else {
            ready_root_cost(&key.root)
        };
        let needed = cost.saturating_add(root_cost);
        if needed > limits::MAX_QUEUE_BYTES {
            state.health.dropped = state.health.dropped.saturating_add(1);
            state.mark_loss(key);
            state.signal(notify);
            return;
        }
        while state.pending.len() >= limits::MAX_QUEUE_DOCUMENTS
            || state.bytes.saturating_add(cost).saturating_add(
                if state.per_root.contains_key(&key.root) {
                    0
                } else {
                    root_cost
                },
            ) > limits::MAX_QUEUE_BYTES
        {
            if !state.evict_one(&key.root) {
                break;
            }
        }
        let root_cost = if state.per_root.contains_key(&key.root) {
            0
        } else {
            ready_root_cost(&key.root)
        };
        if state.pending.len() >= limits::MAX_QUEUE_DOCUMENTS
            || state.bytes.saturating_add(cost).saturating_add(root_cost) > limits::MAX_QUEUE_BYTES
        {
            state.health.dropped = state.health.dropped.saturating_add(1);
            state.mark_loss(key);
            state.signal(notify);
            return;
        }
        if root_cost > 0 {
            state.per_root.insert(key.root.clone(), VecDeque::new());
            state.ready.push_back(key.root.clone());
        }
        state.bytes = state.bytes.saturating_add(cost).saturating_add(root_cost);
        state.clear_loss(&key);
        if let Some(queue) = state.per_root.get_mut(&key.root) {
            queue.push_back(key.clone());
        }
        state.pending.insert(
            key,
            Entry {
                publication: pd,
                cost,
            },
        );
        state.signal(notify);
    }
}

fn retire_locked(state: &mut QueueState, root: &Path, identity: &str, generation: u64) -> bool {
    let Some(streams) = state.active.get_mut(root) else {
        return false;
    };
    if streams.get(identity).map(|s| s.generation) != Some(generation) {
        return false;
    }
    let last_on_root = streams.len() == 1;
    streams.remove(identity); // cost released below
    if last_on_root {
        state.active.remove(root);
    }
    state.metadata_bytes =
        state
            .metadata_bytes
            .saturating_sub(stream_cost(root, identity, last_on_root));
    // When other streams remain on the root, the root's share of the cost
    // stays attributed to them (recompute attributes it to the first).
    let doomed: Vec<DiagnosticKey> = state
        .per_root
        .get(root)
        .map(|queue| {
            queue
                .iter()
                .filter(|key| key.server_identity == identity && key.generation == generation)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    for key in doomed {
        state.remove_pending(&key); // retired stream: nothing to deliver
    }
    let marks: Vec<DiagnosticKey> = state
        .loss_marks
        .iter()
        .filter(|key| {
            key.root == root && key.server_identity == identity && key.generation == generation
        })
        .cloned()
        .collect();
    for key in marks {
        state.clear_loss(&key);
    }
    state.retired_dirty = true;
    true
}

impl DiagnosticsReceiver {
    /// Block until the host must look at the queue, then consume the wake
    /// obligation. Returns `false` once the bus is shut down.
    pub fn wait(&self) -> bool {
        let Some(mut state) = self.shared.lock() else {
            return false;
        };
        while !state.wake_pending && !state.closed {
            state = match self.shared.notify.wait(state) {
                Ok(state) => state,
                Err(_) => return false,
            };
        }
        state.wake_pending = false;
        !state.closed
    }

    /// Re-arm the wake after the host stopped at its per-turn budget with
    /// publications still queued. Returns whether it re-armed.
    pub fn rearm_if_pending(&self) -> bool {
        let Some(mut state) = self.shared.lock() else {
            return false;
        };
        if state.ready.is_empty() {
            return false;
        }
        state.signal(&self.shared.notify);
        true
    }

    /// Pop the next publication, rotating across ready roots.
    pub fn try_recv(&self) -> Result<PublishedDiagnostics, TryRecvError> {
        let Some(mut state) = self.shared.lock() else {
            return Err(TryRecvError::Disconnected);
        };
        // Every iteration removes at least one order entry, so this terminates.
        while let Some(root) = state.ready.pop_front() {
            let Some(key) = state.per_root.get_mut(&root).and_then(VecDeque::pop_front) else {
                if state.per_root.remove(&root).is_some() {
                    state.bytes = state.bytes.saturating_sub(ready_root_cost(&root));
                }
                continue;
            };
            if state.per_root.get(&root).is_some_and(VecDeque::is_empty) {
                state.per_root.remove(&root);
                state.bytes = state.bytes.saturating_sub(ready_root_cost(&root));
            } else {
                // Round-robin: the root goes to the back of the ready ring.
                state.ready.push_back(root);
            }
            let Some(entry) = state.pending.remove(&key) else {
                continue;
            };
            state.bytes = state.bytes.saturating_sub(entry.cost);
            let pd = entry.publication;
            // Retirement purges queued publications, so this is defensive:
            // nothing that is no longer current is ever handed out.
            if !state.is_current(&pd.root, &pd.server_identity, pd.generation) {
                state.health.stale = state.health.stale.saturating_add(1);
                continue;
            }
            return Ok(pd);
        }
        if state.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Whether any publication is queued.
    pub fn has_pending(&self) -> bool {
        self.shared
            .lock()
            .is_some_and(|state| !state.ready.is_empty())
    }

    /// Cumulative health since the last take (the host keeps it sticky).
    pub fn take_health(&self) -> LspHealth {
        self.shared
            .lock()
            .map(|mut state| std::mem::take(&mut state.health))
            .unwrap_or_default()
    }

    pub fn health(&self) -> LspHealth {
        self.shared
            .lock()
            .map(|state| state.health)
            .unwrap_or_default()
    }

    /// Hand loss marks to the host. Document marks move (their metadata is
    /// released here); stream marks are reported and cleared.
    pub fn take_loss_marks(&self) -> LossMarks {
        let Some(mut state) = self.shared.lock() else {
            return LossMarks::default();
        };
        let documents: Vec<DiagnosticKey> = state.loss_marks.drain().collect();
        let released = documents
            .iter()
            .map(diagnostic_key_bytes)
            .fold(0usize, usize::saturating_add);
        state.metadata_bytes = state.metadata_bytes.saturating_sub(released);
        let mut streams = Vec::new();
        for (root, identities) in &mut state.active {
            for (identity, stream) in identities.iter_mut() {
                if std::mem::take(&mut stream.lossy) {
                    streams.push(StreamKey {
                        root: root.clone(),
                        server_identity: identity.clone(),
                        generation: stream.generation,
                    });
                }
            }
        }
        LossMarks { documents, streams }
    }

    /// If any stream was retired since the last call, the set of streams that
    /// are still current — the registration evidence the host store uses to
    /// drop retired streams' retained data. `None` when nothing changed.
    pub fn take_retirements(&self) -> Option<HashSet<StreamKey>> {
        let mut state = self.shared.lock()?;
        if !std::mem::take(&mut state.retired_dirty) {
            return None;
        }
        Some(
            state
                .active
                .iter()
                .flat_map(|(root, identities)| {
                    identities.iter().map(move |(identity, stream)| StreamKey {
                        root: root.clone(),
                        server_identity: identity.clone(),
                        generation: stream.generation,
                    })
                })
                .collect(),
        )
    }

    /// Tear the bus down: free every queued publication, mark and registration
    /// and release the bridge thread.
    pub fn shutdown(&self) {
        if let Some(mut state) = self.shared.lock() {
            let next_generation = state.next_generation;
            *state = QueueState {
                next_generation,
                closed: true,
                ..QueueState::default()
            };
            self.shared.notify.notify_all();
        }
    }

    #[doc(hidden)]
    pub fn footprint(&self) -> QueueFootprint {
        let Some(state) = self.shared.lock() else {
            return QueueFootprint {
                bytes: 0,
                recomputed_bytes: 0,
                metadata_bytes: 0,
                recomputed_metadata_bytes: 0,
                documents: 0,
                active_streams: 0,
            };
        };
        let (recomputed_bytes, recomputed_metadata_bytes) = state.recompute();
        QueueFootprint {
            bytes: state.bytes,
            recomputed_bytes,
            metadata_bytes: state.metadata_bytes,
            recomputed_metadata_bytes,
            documents: state.pending.len(),
            active_streams: state.active.values().map(HashMap::len).sum(),
        }
    }

    #[doc(hidden)]
    pub fn wake_pending(&self) -> bool {
        self.shared.lock().is_some_and(|state| state.wake_pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::LspSeverity;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn diag(message: &str) -> LspDiagnostic {
        LspDiagnostic {
            line: 0,
            character: 0,
            severity: LspSeverity::Error,
            message: message.into(),
            code: None,
            source: None,
        }
    }

    fn pd(
        root: &str,
        identity: &str,
        generation: u64,
        sequence: u64,
        path: &str,
        diagnostics: Vec<LspDiagnostic>,
    ) -> PublishedDiagnostics {
        PublishedDiagnostics {
            root: PathBuf::from(root),
            path: path.into(),
            diagnostics,
            server_identity: identity.into(),
            generation,
            sequence,
            complete: true,
        }
    }

    #[track_caller]
    fn assert_conserved(rx: &DiagnosticsReceiver) -> QueueFootprint {
        let f = rx.footprint();
        assert_eq!(f.bytes, f.recomputed_bytes, "queue bytes drifted: {f:?}");
        assert_eq!(
            f.metadata_bytes, f.recomputed_metadata_bytes,
            "metadata bytes drifted: {f:?}"
        );
        assert!(f.bytes <= limits::MAX_QUEUE_BYTES, "{f:?}");
        assert!(
            f.metadata_bytes <= limits::MAX_QUEUE_METADATA_BYTES,
            "{f:?}"
        );
        assert!(f.documents <= limits::MAX_QUEUE_DOCUMENTS, "{f:?}");
        f
    }

    #[test]
    fn byte_conservation_across_every_admission_outcome() {
        let (tx, rx) = diagnostics_channel();
        let g = tx.register(Path::new("/r"), "rust").unwrap();
        assert_conserved(&rx);
        let mut seq = 0u64;
        let mut next = || {
            seq += 1;
            seq
        };

        // accepted
        tx.publish(pd("/r", "rust", g, next(), "/r/a.rs", vec![diag("one")]));
        let accepted = assert_conserved(&rx);
        assert_eq!(accepted.documents, 1);
        // replaced (larger), then replaced (smaller)
        tx.publish(pd(
            "/r",
            "rust",
            g,
            next(),
            "/r/a.rs",
            vec![diag(&"x".repeat(1000))],
        ));
        assert!(assert_conserved(&rx).bytes > accepted.bytes);
        tx.publish(pd("/r", "rust", g, next(), "/r/a.rs", vec![diag("one")]));
        assert_eq!(assert_conserved(&rx).bytes, accepted.bytes);
        // refused: a single publication larger than the whole queue
        tx.publish(pd(
            "/r",
            "rust",
            g,
            next(),
            "/r/huge.rs",
            vec![diag(&"h".repeat(limits::MAX_QUEUE_BYTES))],
        ));
        assert_eq!(assert_conserved(&rx).bytes, accepted.bytes);
        assert_eq!(rx.health().dropped, 1);
        // stale: unknown generation, wrong identity, old sequence
        tx.publish(pd("/r", "rust", g + 99, next(), "/r/a.rs", vec![diag("s")]));
        tx.publish(pd("/r", "clang", g, next(), "/r/a.rs", vec![diag("s")]));
        tx.publish(pd("/r", "rust", g, 1, "/r/a.rs", vec![diag("s")]));
        assert_eq!(assert_conserved(&rx).bytes, accepted.bytes);
        assert_eq!(rx.health().stale, 3);
        // cleared: an empty publication replaces the pending update
        tx.publish(pd("/r", "rust", g, next(), "/r/a.rs", vec![]));
        assert_conserved(&rx);
        let got = rx.try_recv().unwrap();
        assert!(got.diagnostics.is_empty() && got.complete);
        let empty = assert_conserved(&rx);
        assert_eq!((empty.bytes, empty.documents), (0, 0));
        // evicted: fill by bytes
        let chunk = "e".repeat(40 * 1024);
        for index in 0..limits::MAX_QUEUE_DOCUMENTS {
            tx.publish(pd(
                "/r",
                "rust",
                g,
                next(),
                &format!("/r/{index}.rs"),
                vec![diag(&chunk)],
            ));
            assert_conserved(&rx);
        }
        assert!(rx.health().dropped > 1, "byte-driven eviction happened");
        // retired: purges everything and releases registry metadata
        assert!(tx.retire(Path::new("/r"), "rust", g));
        let retired = assert_conserved(&rx);
        assert_eq!(
            (retired.bytes, retired.documents, retired.metadata_bytes),
            (0, 0, 0)
        );
    }

    #[test]
    fn repeated_stale_floods_near_cap_never_move_accounting() {
        let (tx, rx) = diagnostics_channel();
        let g = tx.register(Path::new("/r"), "rust").unwrap();
        let chunk = "n".repeat(30 * 1024);
        for index in 0..limits::MAX_QUEUE_DOCUMENTS {
            tx.publish(pd(
                "/r",
                "rust",
                g,
                index as u64 + 1,
                &format!("/r/{index}.rs"),
                vec![diag(&chunk)],
            ));
        }
        let near_cap = assert_conserved(&rx);
        assert!(near_cap.bytes > limits::MAX_QUEUE_BYTES / 2);
        for round in 0..2_000u64 {
            tx.publish(pd(
                "/r",
                "rust",
                g + 1 + round,
                u64::MAX,
                "/r/0.rs",
                vec![diag(&chunk)],
            ));
            tx.publish(pd(
                "/gone",
                "rust",
                g,
                u64::MAX,
                "/gone/x.rs",
                vec![diag("x")],
            ));
        }
        assert_eq!(assert_conserved(&rx), near_cap);
        assert_eq!(rx.health().stale, 4_000);
    }

    #[test]
    fn retired_generation_stays_stale_after_reopen_and_releases_capacity() {
        let (tx, rx) = diagnostics_channel();
        let root = Path::new("/wt");
        let g1 = tx.register(root, "rust").unwrap();
        let other = tx.register(root, "clang").unwrap();
        tx.publish(pd("/wt", "rust", g1, 1, "/wt/a.rs", vec![diag("old")]));
        tx.publish(pd("/wt", "clang", other, 1, "/wt/a.rs", vec![diag("c")]));
        assert!(tx.retire(root, "rust", g1));
        let g2 = tx.register(root, "rust").unwrap();
        assert!(g2 > g1);
        // Late old-generation publication (even with a huge sequence) is stale.
        tx.publish(pd(
            "/wt",
            "rust",
            g1,
            u64::MAX,
            "/wt/a.rs",
            vec![diag("late")],
        ));
        // A late close of the old generation does not retire the new one.
        assert!(!tx.retire(root, "rust", g1));
        assert!(tx.is_current(root, "rust", g2));
        tx.publish(pd("/wt", "rust", g2, 1, "/wt/a.rs", vec![diag("new")]));
        let mut seen = Vec::new();
        while let Ok(p) = rx.try_recv() {
            seen.push((
                p.server_identity,
                p.generation,
                p.diagnostics[0].message.clone(),
            ));
        }
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("clang".to_string(), other, "c".to_string()),
                ("rust".to_string(), g2, "new".to_string())
            ],
            "old queued data purged at retire; other server untouched"
        );
        assert_eq!(rx.health().stale, 1);
        assert_conserved(&rx);

        // Thousands of open/close cycles never exhaust roots or metadata.
        for index in 0..2_000 {
            let root = PathBuf::from(format!("/cycle/{index}"));
            let g = tx.register(&root, "rust").expect("capacity is released");
            tx.publish(pd(
                root.to_str().unwrap(),
                "rust",
                g,
                1,
                "/x.rs",
                vec![diag("d")],
            ));
            assert!(tx.retire(&root, "rust", g));
        }
        let f = assert_conserved(&rx);
        assert_eq!(f.active_streams, 2);
        assert_eq!(f.documents, 0);
    }

    #[test]
    fn registration_bounds_are_checked_before_retention() {
        let (tx, rx) = diagnostics_channel();
        let long = "x".repeat(limits::MAX_IDENTITY_BYTES + 1);
        assert!(tx.register(Path::new(&format!("/{long}")), "rust").is_err());
        assert!(tx.register(Path::new("/r"), &long).is_err());
        assert!(tx.register(Path::new("/r"), "").is_err());
        assert!(tx.register(Path::new(""), "rust").is_err());
        for index in 0..limits::MAX_ROOTS {
            tx.register(Path::new(&format!("/root{index}")), "rust")
                .unwrap();
        }
        assert!(tx.register(Path::new("/one-too-many"), "rust").is_err());
        for index in 1..limits::MAX_SERVERS_PER_ROOT {
            tx.register(Path::new("/root0"), &format!("s{index}"))
                .unwrap();
        }
        assert!(tx.register(Path::new("/root0"), "overflow").is_err());
        let f = assert_conserved(&rx);
        assert_eq!(
            f.active_streams,
            limits::MAX_ROOTS + limits::MAX_SERVERS_PER_ROOT - 1
        );
    }

    #[test]
    fn replacements_then_final_clear_deliver_only_the_clear() {
        let (tx, rx) = diagnostics_channel();
        let g = tx.register(Path::new("/r"), "rust").unwrap();
        for seq in 1..=50 {
            tx.publish(pd(
                "/r",
                "rust",
                g,
                seq,
                "/r/a.rs",
                vec![diag(&format!("v{seq}"))],
            ));
        }
        tx.publish(pd("/r", "rust", g, 51, "/r/a.rs", vec![]));
        let got = rx.try_recv().unwrap();
        assert!(got.diagnostics.is_empty() && got.complete);
        assert_eq!(got.sequence, 51);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(rx.health(), LspHealth::default(), "coalescing is not loss");
    }

    #[test]
    fn same_document_under_two_servers_is_two_streams() {
        let (tx, rx) = diagnostics_channel();
        let rust = tx.register(Path::new("/r"), "rust").unwrap();
        let clang = tx.register(Path::new("/r"), "clang").unwrap();
        tx.publish(pd("/r", "rust", rust, 1, "/r/a.c", vec![diag("r")]));
        tx.publish(pd("/r", "clang", clang, 1, "/r/a.c", vec![]));
        let mut got = [rx.try_recv().unwrap(), rx.try_recv().unwrap()];
        got.sort_by(|a, b| a.server_identity.cmp(&b.server_identity));
        assert!(got[0].diagnostics.is_empty() && got[0].server_identity == "clang");
        assert_eq!(got[1].diagnostics[0].message, "r");
    }

    #[test]
    fn quiet_root_progresses_and_is_never_evicted_under_a_unique_document_flood() {
        let (tx, rx) = diagnostics_channel();
        let flood = tx.register(Path::new("/flood"), "rust").unwrap();
        let quiet = tx.register(Path::new("/quiet"), "rust").unwrap();
        for index in 0..100u64 {
            tx.publish(pd(
                "/flood",
                "rust",
                flood,
                index + 1,
                &format!("/flood/{index}.rs"),
                vec![diag("f")],
            ));
        }
        tx.publish(pd(
            "/quiet",
            "rust",
            quiet,
            1,
            "/quiet/only.rs",
            vec![diag("q")],
        ));
        for index in 100..1_000u64 {
            tx.publish(pd(
                "/flood",
                "rust",
                flood,
                index + 1,
                &format!("/flood/{index}.rs"),
                vec![diag("f")],
            ));
        }
        assert_conserved(&rx);
        let first_two: Vec<PathBuf> = (0..2).map(|_| rx.try_recv().unwrap().root).collect();
        assert!(
            first_two.contains(&PathBuf::from("/quiet")),
            "{first_two:?}"
        );
        let marks = rx.take_loss_marks();
        assert!(!marks.is_empty());
        assert!(
            marks
                .documents
                .iter()
                .all(|key| key.root == Path::new("/flood")),
            "only the flooding root loses documents"
        );
        // Evicted = total - retained; every eviction is accounted health.
        let health = rx.health();
        assert_eq!(
            health.dropped as usize,
            1_000 + 1 - limits::MAX_QUEUE_DOCUMENTS
        );
        assert_eq!(health.incomplete, health.dropped);
    }

    #[test]
    fn deterministic_eviction_takes_the_flooding_roots_oldest_document() {
        let (tx, rx) = diagnostics_channel();
        let a = tx.register(Path::new("/a"), "s").unwrap();
        let b = tx.register(Path::new("/b"), "s").unwrap();
        for index in 0..limits::MAX_QUEUE_DOCUMENTS as u64 - 1 {
            tx.publish(pd("/a", "s", a, index + 1, &format!("/a/{index}"), vec![]));
        }
        tx.publish(pd("/b", "s", b, 1, "/b/0", vec![]));
        tx.publish(pd("/b", "s", b, 2, "/b/1", vec![]));
        let marks = rx.take_loss_marks();
        assert_eq!(marks.documents.len(), 1);
        assert_eq!(marks.documents[0].path, "/a/0");
    }

    #[test]
    fn loss_marks_escalate_to_stream_marks_and_complete_admission_clears_them() {
        let (tx, rx) = diagnostics_channel();
        let g = tx.register(Path::new("/r"), "rust").unwrap();
        let total = limits::MAX_QUEUE_DOCUMENTS + limits::MAX_LOSS_MARKS + 10;
        for index in 0..total as u64 {
            tx.publish(pd(
                "/r",
                "rust",
                g,
                index + 1,
                &format!("/r/{index}"),
                vec![],
            ));
            assert_conserved(&rx);
        }
        // Re-publishing a lost document removes its (queue-side) loss mark.
        tx.publish(pd("/r", "rust", g, total as u64 + 1, "/r/0", vec![]));
        let marks = rx.take_loss_marks();
        assert_eq!(marks.documents.len(), limits::MAX_LOSS_MARKS - 1);
        assert!(!marks.documents.iter().any(|key| key.path == "/r/0"));
        assert_eq!(marks.streams.len(), 1, "overflow escalated to the stream");
        assert!(
            rx.take_loss_marks().is_empty(),
            "marks move to the host once"
        );
        assert_conserved(&rx);
    }

    #[test]
    fn wake_obligation_is_single_bounded_and_idle_is_silent() {
        let (tx, rx) = diagnostics_channel();
        let g = tx.register(Path::new("/r"), "rust").unwrap();
        assert!(!rx.wake_pending(), "registration alone does not wake");
        tx.record(LspHealth::default());
        assert!(!rx.wake_pending(), "an empty health report does not wake");
        for seq in 1..=10 {
            tx.publish(pd("/r", "rust", g, seq, &format!("/r/{seq}"), vec![]));
        }
        assert!(rx.wake_pending());
        assert!(rx.wait(), "consumes the one obligation");
        assert!(!rx.wake_pending());
        assert!(rx.rearm_if_pending(), "work queued ⇒ re-arm");
        assert!(rx.wait());
        while rx.try_recv().is_ok() {}
        assert!(!rx.rearm_if_pending(), "empty ⇒ no re-arm");
        assert!(!rx.wake_pending());

        // A real bridge: wakes only for work, zero wakes when idle, exits on shutdown.
        let wakes = Arc::new(AtomicUsize::new(0));
        let bridge = {
            let rx = rx.clone();
            let wakes = wakes.clone();
            std::thread::spawn(move || {
                while rx.wait() {
                    wakes.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(wakes.load(Ordering::SeqCst), 0, "idle bus never wakes");
        tx.publish(pd("/r", "rust", g, 100, "/r/x", vec![]));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while wakes.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        while rx.try_recv().is_ok() {}
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "drained bus goes quiet");
        rx.shutdown();
        bridge.join().unwrap();
        let f = rx.footprint();
        assert_eq!((f.bytes, f.metadata_bytes, f.active_streams), (0, 0, 0));
        assert!(tx.register(Path::new("/r"), "rust").is_err(), "closed bus");
    }

    #[test]
    fn concurrent_producers_never_strand_queued_work_without_a_wake() {
        let (tx, rx) = diagnostics_channel();
        let roots: Vec<(String, u64)> = (0..4)
            .map(|index| {
                let root = format!("/p{index}");
                let g = tx.register(Path::new(&root), "rust").unwrap();
                (root, g)
            })
            .collect();
        let producers: Vec<_> = roots
            .into_iter()
            .map(|(root, g)| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for seq in 1..=500u64 {
                        tx.publish(pd(
                            &root,
                            "rust",
                            g,
                            seq,
                            &format!("{root}/{}", seq % 97),
                            vec![],
                        ));
                    }
                })
            })
            .collect();
        // Consumer = the bridge + host loop: block on the wake obligation,
        // drain one budgeted slice of 8, re-arm only when cut short. A lost
        // wake strands work and the deadline below fails.
        let consumer = {
            let rx = rx.clone();
            std::thread::spawn(move || {
                let mut delivered = 0usize;
                let mut wakes = 0usize;
                while rx.wait() {
                    wakes += 1;
                    let mut taken = 0;
                    while taken < 8 && rx.try_recv().is_ok() {
                        taken += 1;
                    }
                    if taken == 8 {
                        rx.rearm_if_pending(); // re-arm result is implied by has_pending
                    }
                    delivered += taken;
                }
                (delivered, wakes)
            })
        };
        for producer in producers {
            producer.join().unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while rx.has_pending() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!rx.has_pending(), "queued work stranded without a wake");
        assert_conserved(&rx);
        rx.shutdown();
        let (delivered, wakes) = consumer.join().unwrap();
        assert!(delivered > 0);
        // One bounded obligation: never more wakes than admissions + re-arms.
        assert!(wakes <= 4 * 500 + delivered / 8 + 1, "wakes={wakes}");
    }
}
