//! Loop-local supervisor for live browser-preview targets.
//!
//! Discovery facts arrive from existing event sources: bounded pane output,
//! pane exit/EOF, one-shot config/package scans, and sandbox forward events.
//! The supervisor is memory-only; forward DB rows remain the forwarder's cache,
//! never a second preview source of truth.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use thegn_core::preview::{
    PortHint, PortHintSource, PreviewStatus, PreviewTarget, parse_pane_port_hints_wrapped,
    select_target,
};

use crate::chrome::PreviewView;

/// Pane diagnostic bytes retained per pane. This matches the pure parser's
/// maximum input so repeated output can never grow host memory without bound.
const DIAGNOSTIC_TAIL_BYTES: usize = thegn_core::preview::MAX_PORT_HINT_CHARS;

/// The retained buffer may grow to twice the diagnostic bound before it is
/// front-trimmed back to [`DIAGNOSTIC_TAIL_BYTES`], so the trim memmove is
/// amortized over at least `DIAGNOSTIC_TAIL_BYTES` of output instead of being
/// paid on every chunk once the tail is full.
const TAIL_TRIM_THRESHOLD: usize = 2 * DIAGNOSTIC_TAIL_BYTES;

/// Minimum look-back before the line holding the first new byte. It keeps a
/// soft-wrapped (or right-margin redrawn) previous row inside the parse window
/// so a URL split across the wrap is still joined.
const WRAP_OVERLAP_MIN_BYTES: usize = 1024;

/// A single unbroken line longer than this is not re-parsed from its start on
/// every chunk; the window falls back to the wrap overlap before the new bytes.
const MAX_LINE_LOOKBACK_BYTES: usize = 8 * 1024;

/// One pane's retained (ANSI-stripped) output. Shared with the same-process
/// diagnostic snapshot so a published snapshot stays live as output arrives
/// without re-copying the tail on every chunk.
type SharedTail = Arc<Mutex<Vec<u8>>>;

/// What the response snapshot serves for one worktree.
#[derive(Debug, Clone)]
enum DiagnosticSource {
    Pane(SharedTail),
    Manifest(String),
}

impl DiagnosticSource {
    fn render(&self) -> String {
        match self {
            Self::Pane(tail) => {
                String::from_utf8_lossy(bounded_tail(&lock_tail(tail))).into_owned()
            }
            Self::Manifest(text) => text.clone(),
        }
    }
}

/// Same-process response snapshot. A detached daemon legitimately has no
/// compositor-side pane diagnostics and therefore leaves this empty.
static DIAGNOSTIC_SNAPSHOTS: OnceLock<Mutex<HashMap<String, DiagnosticSource>>> = OnceLock::new();

pub(crate) fn diagnostic_snapshot(worktree: &str) -> Option<String> {
    let source = DIAGNOSTIC_SNAPSHOTS
        .get()
        .and_then(|snapshots| snapshots.lock().ok()?.get(worktree).cloned())?;
    Some(source.render())
}

/// A poisoned tail only means a panic elsewhere while appending bytes; the
/// buffer is still a valid byte vector, so keep serving it.
fn lock_tail(tail: &Mutex<Vec<u8>>) -> MutexGuard<'_, Vec<u8>> {
    tail.lock().unwrap_or_else(PoisonError::into_inner)
}

/// First char boundary at or after `index` (skips at most three UTF-8
/// continuation bytes, so invalid input cannot stall the scan).
fn ceil_char_boundary(bytes: &[u8], index: usize) -> usize {
    let mut index = index.min(bytes.len());
    let mut skipped = 0;
    while skipped < 3 && bytes.get(index).is_some_and(|byte| byte & 0xC0 == 0x80) {
        index += 1;
        skipped += 1;
    }
    index
}

/// The diagnostic view of a retained tail: at most [`DIAGNOSTIC_TAIL_BYTES`],
/// starting on a char boundary.
fn bounded_tail(tail: &[u8]) -> &[u8] {
    let start = ceil_char_boundary(tail, tail.len().saturating_sub(DIAGNOSTIC_TAIL_BYTES));
    &tail[start..]
}

fn is_line_break(byte: u8) -> bool {
    byte == b'\n' || byte == b'\r'
}

/// Start of the slice of `tail` that must be re-parsed after its last
/// `new_len` bytes arrived.
///
/// The window opens at the start of the line holding the first new byte,
/// extended back by the wrap overlap (`max(2 * cols, 1024)` bytes) and snapped
/// forward to the first line start inside that overlap. Hints wholly before
/// the window were already found by an earlier chunk; a hint that was still
/// incomplete at the previous tail end lies on the line of the first new byte
/// and is therefore re-parsed. A single line longer than
/// `MAX_LINE_LOOKBACK_BYTES` instead starts the overlap before the new bytes,
/// on a char boundary. The window never exceeds [`DIAGNOSTIC_TAIL_BYTES`].
fn parse_window_start(tail: &[u8], new_len: usize, cols: Option<u16>) -> usize {
    let first_new = tail.len().saturating_sub(new_len);
    let overlap = cols
        .map_or(0, |columns| 2 * usize::from(columns))
        .max(WRAP_OVERLAP_MIN_BYTES);
    let line_start = tail[..first_new]
        .iter()
        .rposition(|byte| is_line_break(*byte))
        .map_or(0, |index| index + 1);
    let start = if first_new - line_start > MAX_LINE_LOOKBACK_BYTES {
        ceil_char_boundary(tail, first_new.saturating_sub(overlap))
    } else {
        let cut = line_start.saturating_sub(overlap);
        if cut == 0 {
            0
        } else {
            // A break at `cut - 1` makes `cut` itself a line start. A line
            // start follows an ASCII byte, so it is always a char boundary.
            tail[cut - 1..line_start]
                .iter()
                .position(|byte| is_line_break(*byte))
                .map_or(line_start, |index| cut + index)
        }
    };
    start.max(ceil_char_boundary(
        tail,
        tail.len().saturating_sub(DIAGNOSTIC_TAIL_BYTES),
    ))
}

#[derive(Debug, Default)]
struct Candidate {
    configured: Option<PortHint>,
    package: Option<PortHint>,
    watched_hint: Option<PortHint>,
    panes: BTreeMap<u32, (PortHint, Option<String>)>,
    ended_panes: BTreeMap<u32, (PortHint, Option<String>)>,
    forwarded_url: Option<String>,
    provider_ended: bool,
    watched: bool,
    generation: u64,
    probe_inflight: bool,
    probe_spent: bool,
    probe_ok: bool,
}

impl Candidate {
    fn target(&self, worktree: &str, port: u16) -> Option<PreviewTarget> {
        let (hint, pane, session) = if let Some(hint) = &self.configured {
            let pane_fact = self.panes.iter().next();
            (
                hint,
                pane_fact.map(|(id, _)| id.to_string()),
                pane_fact.and_then(|(_, (_, session))| session.clone()),
            )
        } else if let Some((id, (hint, session))) = self.panes.iter().next() {
            (hint, Some(id.to_string()), session.clone())
        } else if let Some((id, (hint, session))) = self.ended_panes.iter().next() {
            (hint, Some(id.to_string()), session.clone())
        } else if let Some(hint) = &self.watched_hint {
            (hint, None, None)
        } else if let Some(hint) = &self.package {
            (hint, None, None)
        } else {
            return None;
        };
        let status = if self.forwarded_url.is_some() || self.probe_ok {
            PreviewStatus::Up
        } else if !self.panes.is_empty() {
            PreviewStatus::Seen
        } else if self.provider_ended || !self.ended_panes.is_empty() {
            PreviewStatus::Down
        } else {
            PreviewStatus::Unknown
        };
        Some(PreviewTarget {
            worktree: worktree.to_string(),
            port,
            url: self.forwarded_url.clone().unwrap_or_else(|| hint.url()),
            source: hint.source,
            pane,
            session,
            status,
        })
    }

    fn empty(&self) -> bool {
        self.configured.is_none()
            && self.package.is_none()
            && self.watched_hint.is_none()
            && self.panes.is_empty()
            && self.ended_panes.is_empty()
            && self.forwarded_url.is_none()
            && !self.provider_ended
    }
}

/// One local loopback target submitted to an off-loop probe batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeRequest {
    pub worktree: String,
    pub port: u16,
    pub host: String,
    pub generation: u64,
}

/// One terminal result from all retries for a probe request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeResult {
    pub request: ProbeRequest,
    pub ok: bool,
}

/// Owns preview target lifecycle and bounded diagnostic tails for this UI
/// process. All mutation happens on the event loop.
#[derive(Default)]
pub(crate) struct PreviewSupervisor {
    enabled: bool,
    candidates: BTreeMap<(String, u16), Candidate>,
    pane_tails: HashMap<u32, SharedTail>,
    pane_strippers: HashMap<u32, thegn_core::history::AnsiStripper>,
    pane_worktrees: HashMap<u32, String>,
    latest_scan_generation: u64,
    diagnostics: HashMap<String, String>,
    last_probe_worktree: Option<String>,
    /// Test hook: how many times `publish_diagnostic` ran.
    #[cfg(test)]
    publishes: std::sync::atomic::AtomicUsize,
}

impl PreviewSupervisor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.candidates.clear();
            self.pane_tails.clear();
            self.pane_strippers.clear();
            self.pane_worktrees.clear();
            self.diagnostics.clear();
            if let Some(snapshots) = DIAGNOSTIC_SNAPSHOTS.get()
                && let Ok(mut snapshots) = snapshots.lock()
            {
                snapshots.clear();
            }
        }
    }

    /// Advance and return the generation for a one-shot static scan.
    pub(crate) fn next_scan_generation(&mut self) -> u64 {
        self.latest_scan_generation = self.latest_scan_generation.wrapping_add(1);
        self.latest_scan_generation
    }

    /// Apply only the newest requested scan. Static evidence is replaced for
    /// this worktree; pane/provider lifecycle evidence remains live.
    pub(crate) fn apply_scan(&mut self, scan: crate::preview_watch::ScanResult) -> bool {
        if scan.generation != self.latest_scan_generation {
            return false;
        }
        let before = self.view(&scan.worktree);
        for ((worktree, _), candidate) in &mut self.candidates {
            if worktree == &scan.worktree {
                Self::rearm(candidate);
                candidate.configured = None;
                candidate.package = None;
            }
        }
        for hint in scan.configured {
            let port = hint.port;
            let candidate = self
                .candidates
                .entry((scan.worktree.clone(), port))
                .or_default();
            Self::rearm(candidate);
            candidate.configured = Some(hint);
        }
        for hint in scan.package {
            let port = hint.port;
            self.candidates
                .entry((scan.worktree.clone(), port))
                .or_default()
                .package = Some(hint);
        }
        self.candidates.retain(|_, candidate| !candidate.empty());
        if let Some(diagnostic) = scan.diagnostic {
            self.diagnostics.insert(scan.worktree.clone(), diagnostic);
        } else {
            self.diagnostics.remove(&scan.worktree);
        }
        self.publish_diagnostic(&scan.worktree);
        before != self.view(&scan.worktree)
    }

    /// Feed one pane's output to the pure core parser. The pane keeps a
    /// bounded diagnostic tail, but each chunk re-parses only the window that
    /// can hold a new or newly-completed hint (see `parse_window_start`).
    /// Pane facts persist after their URL leaves the window or the tail and
    /// are removed only when a longer digit-prefix hint from that pane
    /// supersedes them. Returns true only when the selected render projection
    /// changed; a chunk that changes no candidate does no view work.
    pub(crate) fn pane_output(
        &mut self,
        pane_id: u32,
        worktree: &str,
        session: Option<String>,
        bytes: &[u8],
        cols: Option<u16>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        // Strip incrementally before retaining the bounded tail. Keeping the
        // stripper state per pane matters when an OSC/DCS sequence spans chunks
        // or exceeds the tail bound: once the opener is evicted, hidden payload
        // bytes must not become parser-visible plain text.
        let mut visible = Vec::with_capacity(bytes.len());
        self.pane_strippers
            .entry(pane_id)
            .or_default()
            .strip_into(bytes, &mut visible);
        if self.pane_worktrees.get(&pane_id).map(String::as_str) != Some(worktree) {
            self.pane_worktrees.insert(pane_id, worktree.to_string());
        }
        if visible.is_empty() {
            // Nothing parser-visible arrived, so no hint can have appeared or
            // been completed.
            return false;
        }
        let window = {
            let mut tail = lock_tail(self.pane_tails.entry(pane_id).or_default());
            tail.extend_from_slice(&visible);
            if tail.len() > TAIL_TRIM_THRESHOLD {
                let excess = tail.len() - DIAGNOSTIC_TAIL_BYTES;
                tail.drain(..excess);
            }
            let start = parse_window_start(&tail, visible.len(), cols);
            String::from_utf8_lossy(&tail[start..]).into_owned()
        };
        let hints = parse_pane_port_hints_wrapped(&window, cols);
        if hints.is_empty() {
            // No new port means nothing to insert and nothing to supersede.
            return false;
        }
        let new_ports: std::collections::BTreeSet<u16> =
            hints.iter().map(|hint| hint.port).collect();
        // Keep pane facts after their URL scrolls out of the window. A new
        // hint only supersedes an old one when it extends that old port's
        // digits (for example, an echoed `:87` becoming `:8765`).
        let superseded: Vec<u16> = self
            .candidates
            .iter()
            .filter(|((candidate_worktree, _), candidate)| {
                candidate_worktree == worktree && candidate.panes.contains_key(&pane_id)
            })
            .map(|((_, port), _)| *port)
            .filter(|old| {
                !new_ports.contains(old)
                    && new_ports
                        .iter()
                        .any(|new| new != old && new.to_string().starts_with(&old.to_string()))
            })
            .collect();
        let key = |port: u16| (worktree.to_string(), port);
        let inserts_change = hints.iter().any(|hint| {
            self.candidates
                .get(&key(hint.port))
                .is_none_or(|candidate| {
                    !candidate.ended_panes.is_empty()
                        || candidate
                            .panes
                            .get(&pane_id)
                            .is_none_or(|(old_hint, old_session)| {
                                old_hint != hint || old_session != &session
                            })
                })
        });
        if superseded.is_empty() && !inserts_change {
            // Re-inserting unchanged hints is a no-op: skip the view
            // projection and the diagnostic publish entirely.
            return false;
        }
        let before = self.view(worktree);
        for port in superseded {
            if let Some(candidate) = self.candidates.get_mut(&key(port)) {
                candidate.panes.remove(&pane_id);
                Self::rearm(candidate);
            }
        }
        for hint in hints {
            let candidate = self.candidates.entry(key(hint.port)).or_default();
            // A new live watcher supersedes ended evidence for this target;
            // keep exit history bounded by the concurrently-observed panes.
            candidate.ended_panes.clear();
            let changed = candidate
                .panes
                .get(&pane_id)
                .is_none_or(|(old_hint, old_session)| old_hint != &hint || old_session != &session);
            candidate.panes.insert(pane_id, (hint, session.clone()));
            if changed {
                Self::rearm(candidate);
            }
        }
        self.publish_diagnostic(worktree);
        before != self.view(worktree)
    }

    fn rearm(candidate: &mut Candidate) {
        candidate.generation = candidate.generation.wrapping_add(1).max(1);
        candidate.probe_inflight = false;
        candidate.probe_spent = false;
        candidate.probe_ok = false;
    }

    /// Memory-only Forward-panel watch. This creates an unknown candidate that
    /// is eligible for the same local probe as configured candidates.
    pub(crate) fn watch(&mut self, worktree: &str, port: u16) -> bool {
        if port == 0 {
            return false;
        }
        let candidate = self
            .candidates
            .entry((worktree.to_string(), port))
            .or_default();
        if candidate.watched {
            return false;
        }
        candidate.watched = true;
        candidate.watched_hint = Some(PortHint {
            port,
            host: "localhost".into(),
            source: PortHintSource::PackageScript,
        });
        Self::rearm(candidate);
        true
    }

    pub(crate) fn unwatch(&mut self, worktree: &str, port: u16) -> bool {
        let Some(candidate) = self.candidates.get_mut(&(worktree.to_string(), port)) else {
            return false;
        };
        if !candidate.watched {
            return false;
        }
        candidate.watched = false;
        candidate.watched_hint = None;
        Self::rearm(candidate);
        if candidate.empty() {
            self.candidates.remove(&(worktree.to_string(), port));
        }
        true
    }

    pub(crate) fn is_watched(&self, worktree: &str, port: u16) -> bool {
        self.candidates
            .get(&(worktree.to_string(), port))
            .is_some_and(|candidate| candidate.watched)
    }

    pub(crate) fn active_worktree(&mut self, worktree: &str) {
        if self
            .last_probe_worktree
            .as_deref()
            .is_some_and(|last| last != worktree)
        {
            for ((candidate_worktree, _), candidate) in &mut self.candidates {
                if candidate_worktree == worktree {
                    Self::rearm(candidate);
                }
            }
        }
        self.last_probe_worktree = Some(worktree.to_string());
    }

    pub(crate) fn probe_requests(&mut self, worktree: &str) -> Vec<ProbeRequest> {
        self.active_worktree(worktree);
        let keys: Vec<(String, u16)> = self
            .candidates
            .iter()
            .filter_map(|(key, candidate)| {
                let target = candidate.target(worktree, key.1)?;
                (key.0 == worktree
                    && (!candidate.panes.is_empty()
                        || candidate.configured.is_some()
                        || candidate.watched)
                    && matches!(target.status, PreviewStatus::Seen | PreviewStatus::Unknown)
                    && !candidate.probe_inflight
                    && !candidate.probe_spent)
                    .then(|| key.clone())
            })
            .collect();
        let mut requests = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(candidate) = self.candidates.get_mut(&key) {
                candidate.probe_inflight = true;
                requests.push(ProbeRequest {
                    worktree: key.0,
                    port: key.1,
                    host: candidate
                        .panes
                        .values()
                        .next()
                        .map(|(hint, _)| hint.host.clone())
                        .or_else(|| candidate.configured.as_ref().map(|hint| hint.host.clone()))
                        .unwrap_or_else(|| "localhost".into()),
                    generation: candidate.generation,
                });
            }
        }
        requests
    }

    pub(crate) fn rearm_port(&mut self, worktree: &str, port: u16) {
        if let Some(candidate) = self.candidates.get_mut(&(worktree.to_string(), port)) {
            Self::rearm(candidate);
        }
    }

    /// Snapshot the selected target for an open/copy action, then re-arm its
    /// probe. The returned source and status are what the user acted on; the
    /// pending probe may update them on a later loop iteration.
    pub(crate) fn open_view(&mut self, worktree: &str) -> Option<PreviewView> {
        let view = self.view(worktree)?;
        self.rearm_port(&view.worktree, view.port);
        Some(view)
    }

    pub(crate) fn apply_probe(&mut self, result: ProbeResult) -> bool {
        let worktree = result.request.worktree.clone();
        let before_view = self.view(&worktree);
        let Some(candidate) = self
            .candidates
            .get_mut(&(result.request.worktree.clone(), result.request.port))
        else {
            return false;
        };
        if candidate.generation != result.request.generation {
            return false;
        }
        candidate.probe_inflight = false;
        candidate.probe_spent = true;
        candidate.probe_ok = result.ok;
        before_view != self.view(&worktree)
    }

    pub(crate) fn has_live_pane_evidence(&self, worktree: &str) -> bool {
        self.candidates
            .iter()
            .any(|((candidate_worktree, _), candidate)| {
                candidate_worktree == worktree && !candidate.panes.is_empty()
            })
    }

    /// Record PTY EOF/child exit and transition its targets to `down` when no
    /// other pane/provider still proves reachability.
    pub(crate) fn pane_exit(&mut self, pane_id: u32) -> bool {
        let Some(worktree) = self.pane_worktrees.remove(&pane_id) else {
            self.pane_tails.remove(&pane_id);
            return false;
        };
        let before = self.view(&worktree);
        self.pane_tails.remove(&pane_id);
        self.pane_strippers.remove(&pane_id);
        for ((candidate_worktree, _), candidate) in &mut self.candidates {
            if candidate_worktree == &worktree
                && let Some(fact) = candidate.panes.remove(&pane_id)
            {
                candidate.ended_panes.insert(pane_id, fact);
                Self::rearm(candidate);
            }
        }
        self.publish_diagnostic(&worktree);
        before != self.view(&worktree)
    }

    /// Existing sandbox provider/forward events are the watch source for an
    /// otherwise unreachable container-local target.
    pub(crate) fn provider_up(&mut self, worktree: &str, port: u16, url: String) -> bool {
        if !self.enabled {
            return false;
        }
        let before = self.view(worktree);
        let candidate = self
            .candidates
            .entry((worktree.to_string(), port))
            .or_default();
        candidate.provider_ended = false;
        candidate.forwarded_url = Some(url);
        Self::rearm(candidate);
        // A provider event may be the first discovery fact (for a process that
        // printed no URL); represent it as an honest config-shaped loopback
        // candidate while the provider supplies the `up` lifecycle proof.
        if candidate.configured.is_none()
            && candidate.package.is_none()
            && candidate.panes.is_empty()
        {
            candidate.configured = PortHint::configured(port);
        }
        before != self.view(worktree)
    }

    pub(crate) fn provider_down(&mut self, worktree: &str, port: u16) -> bool {
        let before = self.view(worktree);
        if let Some(candidate) = self.candidates.get_mut(&(worktree.to_string(), port)) {
            candidate.forwarded_url = None;
            candidate.provider_ended = true;
            Self::rearm(candidate);
        }
        before != self.view(worktree)
    }

    pub(crate) fn view(&self, worktree: &str) -> Option<PreviewView> {
        if !self.enabled {
            return None;
        }
        let targets: Vec<PreviewTarget> = self
            .candidates
            .iter()
            .filter(|((candidate_worktree, _), _)| candidate_worktree == worktree)
            .filter_map(|((_, port), candidate)| candidate.target(worktree, *port))
            .collect();
        select_target(&targets).map(PreviewView::from)
    }

    /// Bounded diagnostic context for the selected target. Pane output wins;
    /// otherwise return the latest one-shot manifest error. This is the seam
    /// consumed by the response/fetch layer without exposing emulator state.
    #[allow(dead_code)] // chunk 3 consumes this response-context seam
    pub(crate) fn diagnostic(&self, worktree: &str) -> Option<String> {
        self.diagnostic_source(worktree)
            .map(|source| source.render())
    }

    fn diagnostic_source(&self, worktree: &str) -> Option<DiagnosticSource> {
        let pane_tail = self.view(worktree).and_then(|view| {
            self.candidates
                .get(&(worktree.to_string(), view.port))
                .and_then(|candidate| candidate.panes.keys().next())
                .and_then(|pane| self.pane_tails.get(pane))
                .map(|tail| DiagnosticSource::Pane(Arc::clone(tail)))
        });
        pane_tail.or_else(|| {
            self.diagnostics
                .get(worktree)
                .map(|text| DiagnosticSource::Manifest(text.clone()))
        })
    }

    /// Bounded diagnostic length of one pane's retained tail.
    #[cfg(test)]
    fn pane_tail_len(&self, pane_id: u32) -> usize {
        self.pane_tails
            .get(&pane_id)
            .map_or(0, |tail| bounded_tail(&lock_tail(tail)).len())
    }

    /// Point the response snapshot at this worktree's current diagnostic
    /// source. A pane source shares the live tail, so this only needs to run
    /// when the selected candidate (or the manifest diagnostic) can change,
    /// not on every output chunk.
    fn publish_diagnostic(&self, worktree: &str) {
        #[cfg(test)]
        self.publishes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let snapshots = DIAGNOSTIC_SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()));
        let Ok(mut snapshots) = snapshots.lock() else {
            return;
        };
        match self.diagnostic_source(worktree) {
            Some(diagnostic) => {
                snapshots.insert(worktree.to_string(), diagnostic);
            }
            None => {
                snapshots.remove(worktree);
            }
        }
    }
}

impl From<&PreviewTarget> for PreviewView {
    fn from(target: &PreviewTarget) -> Self {
        Self {
            worktree: target.worktree.clone(),
            port: target.port,
            url: target.url.clone(),
            source: target.source,
            status: target.status,
        }
    }
}

/// Short suffix for copy/open feedback, naming the last known lifecycle state.
/// `up` needs no qualifier; `down` is a stopped server; the remaining states
/// have no reachability proof yet.
pub(crate) fn status_note(status: PreviewStatus) -> &'static str {
    match status {
        PreviewStatus::Up => "",
        PreviewStatus::Down => " (server stopped)",
        PreviewStatus::Seen | PreviewStatus::Unknown => " (not answering yet)",
    }
}

/// Whether this local worktree's pane can be reached through host loopback.
/// An empty observed backend is the ordinary never-sandboxed host case when
/// sandboxing is disabled; with sandboxing enabled it is trusted only after a
/// pane has produced live output. Remote placements and unknown backend names
/// stay fail-closed.
pub(crate) fn probe_allowed(
    label: &str,
    placement: Option<&str>,
    sandbox_enabled: bool,
    network: thegn_core::config::Network,
    live_pane_evidence: bool,
) -> bool {
    if placement.is_some() {
        return false;
    }
    let Some(backend) = thegn_core::sandbox::Backend::parse(label) else {
        return label.is_empty() && (!sandbox_enabled || live_pane_evidence);
    };
    backend == thegn_core::sandbox::Backend::None
        || (network != thegn_core::config::Network::None
            && (network == thegn_core::config::Network::Host
                || thegn_core::sandbox_matrix::row(
                    backend,
                    thegn_core::sandbox_backend::host_os(),
                )
                .net == thegn_core::sandbox_matrix::NetIsolation::WhenSealed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn pane_lifecycle_discovers_up_then_down_and_bounds_tail() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(
            7,
            "/repo",
            Some("session".into()),
            b"ready http://localhost:5173/\n",
            Some(80)
        ));
        let view = sup.view("/repo").unwrap();
        assert_eq!(view.port, 5173);
        assert_eq!(view.status, PreviewStatus::Seen);
        assert_eq!(view.source, thegn_core::preview::PortHintSource::PaneOutput);
        assert!(sup.pane_tail_len(7) <= DIAGNOSTIC_TAIL_BYTES);
        assert!(sup.diagnostic("/repo").unwrap().contains("localhost:5173"));

        assert!(sup.pane_exit(7));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Down);
    }

    #[test]
    fn hidden_terminal_payload_cannot_become_a_port_hint_after_tail_truncation() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        let mut hidden = b"\x1b]0;".to_vec();
        hidden.extend(std::iter::repeat_n(b'x', DIAGNOSTIC_TAIL_BYTES + 32));
        hidden.extend_from_slice(b" http://localhost:6666");
        assert!(!sup.pane_output(7, "/repo", None, &hidden, Some(80)));
        assert!(sup.view("/repo").is_none());

        assert!(sup.pane_output(
            7,
            "/repo",
            None,
            b"\x07ready http://localhost:5173/\n",
            Some(80)
        ));
        assert_eq!(sup.view("/repo").unwrap().port, 5173);
    }

    #[test]
    fn static_and_provider_facts_project_unknown_up_down() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        let generation = sup.next_scan_generation();
        assert!(sup.apply_scan(crate::preview_watch::ScanResult {
            generation,
            worktree: "/repo".into(),
            configured: vec![PortHint::configured(3000).unwrap()],
            package: Vec::new(),
            diagnostic: None,
        }));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Unknown);
        assert!(sup.provider_up("/repo", 3000, "http://localhost:4100".into()));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Up);
        assert!(sup.provider_down("/repo", 3000));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Down);
    }

    #[test]
    fn stale_scans_are_ignored() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        let stale = sup.next_scan_generation();
        let _current = sup.next_scan_generation();
        assert!(!sup.apply_scan(crate::preview_watch::ScanResult {
            generation: stale,
            worktree: "/repo".into(),
            configured: vec![PortHint::configured(3000).unwrap()],
            package: Vec::new(),
            diagnostic: None,
        }));
        assert!(sup.view("/repo").is_none());
    }

    #[test]
    fn per_keystroke_echo_keeps_only_the_full_port() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        for byte in b"http://localhost:8765/\n" {
            sup.pane_output(9, "/repo", None, &[*byte], Some(20));
        }
        assert_eq!(
            sup.candidates
                .keys()
                .map(|(_, port)| *port)
                .collect::<Vec<_>>(),
            vec![8765]
        );
    }

    #[test]
    fn configured_port_visible_before_output() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        let generation = sup.next_scan_generation();
        assert!(sup.apply_scan(crate::preview_watch::ScanResult {
            generation,
            worktree: "/repo".into(),
            configured: vec![PortHint::configured(8765).unwrap()],
            package: Vec::new(),
            diagnostic: None,
        }));
        let view = sup.view("/repo").unwrap();
        assert_eq!(view.status, PreviewStatus::Unknown);
        assert_eq!(
            thegn_core::preview::status_label(view.status, view.source),
            "configured"
        );
        assert_eq!(sup.probe_requests("/repo").len(), 1);
    }

    #[test]
    fn pane_seen_is_seen_until_probe() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        sup.pane_output(2, "/repo", None, b"http://localhost:8765/\n", Some(80));
        let req = sup.probe_requests("/repo").pop().unwrap();
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Seen);
        assert!(sup.apply_probe(ProbeResult {
            request: req,
            ok: true
        }));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Up);
    }

    #[test]
    fn probe_reachability_is_fail_closed_for_remote_and_isolated_targets() {
        use thegn_core::config::Network;

        assert!(probe_allowed("", None, false, Network::Host, false));
        assert!(probe_allowed("", None, true, Network::Host, true));
        assert!(!probe_allowed("", None, true, Network::Host, false));
        // OCI Network::Host maps to `--network host`, which shares the host
        // loopback namespace even for the rootless Podman backend.
        assert!(probe_allowed(
            "podman-rootless",
            None,
            true,
            Network::Host,
            true
        ));
        assert!(!probe_allowed("", Some("ssh"), false, Network::Host, true));
        assert!(!probe_allowed("bwrap", None, true, Network::None, true));
    }

    #[test]
    fn evicted_pane_url_remains_the_preview_target() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(17, "/repo", None, b"http://localhost:5173/\n", Some(80)));
        let logs = vec![b'x'; DIAGNOSTIC_TAIL_BYTES + 128];
        sup.pane_output(17, "/repo", None, &logs, Some(80));
        assert_eq!(sup.view("/repo").unwrap().port, 5173);
    }

    #[test]
    fn failed_reprobe_changes_up_view_back_to_seen() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        sup.pane_output(22, "/repo", None, b"http://localhost:8765/\n", Some(80));
        let request = sup.probe_requests("/repo").pop().unwrap();
        assert!(sup.apply_probe(ProbeResult { request, ok: true }));
        let up_view = sup.view("/repo").unwrap();
        assert_eq!(up_view.status, PreviewStatus::Up);

        sup.rearm_port("/repo", 8765);
        let request = sup.probe_requests("/repo").pop().unwrap();
        assert!(!sup.apply_probe(ProbeResult { request, ok: false }));
        let seen_view = sup.view("/repo").unwrap();
        assert_eq!(seen_view.status, PreviewStatus::Seen);
        assert_ne!(up_view, seen_view);
    }

    #[test]
    fn probe_change_result_tracks_the_visible_projection() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        sup.provider_up("/repo", 8765, "http://localhost:9876".into());
        let before = sup.view("/repo").unwrap();
        let generation = sup.candidates[&(String::from("/repo"), 8765)].generation;
        assert!(!sup.apply_probe(ProbeResult {
            request: ProbeRequest {
                worktree: "/repo".into(),
                port: 8765,
                host: "localhost".into(),
                generation,
            },
            ok: true,
        }));
        assert_eq!(sup.view("/repo").unwrap(), before);
    }

    #[test]
    fn probe_requests_are_bounded_and_rearmed() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        sup.watch("/repo", 8765);
        let first = sup.probe_requests("/repo");
        assert_eq!(first.len(), 1);
        assert!(sup.probe_requests("/repo").is_empty());
        assert!(!sup.apply_probe(ProbeResult {
            request: ProbeRequest {
                generation: first[0].generation + 1,
                ..first[0].clone()
            },
            ok: true
        }));
        assert!(!sup.watch("/repo", 8765));
        sup.rearm_port("/repo", 8765);
        let second = sup.probe_requests("/repo");
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].generation, second[0].generation);
        assert!(!sup.apply_probe(ProbeResult {
            request: second[0].clone(),
            ok: false
        }));
        assert!(sup.probe_requests("/repo").is_empty());
    }

    #[test]
    fn status_note_names_the_lifecycle_without_inventing_proof() {
        assert_eq!(status_note(PreviewStatus::Up), "");
        assert_eq!(status_note(PreviewStatus::Seen), " (not answering yet)");
        assert_eq!(status_note(PreviewStatus::Unknown), " (not answering yet)");
        assert_eq!(status_note(PreviewStatus::Down), " (server stopped)");
    }

    #[test]
    fn open_view_preserves_the_acted_on_status_and_rearms_one_probe() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        sup.pane_output(30, "/repo", None, b"http://localhost:8765/\n", Some(80));
        let request = sup.probe_requests("/repo").pop().unwrap();
        assert!(sup.apply_probe(ProbeResult { request, ok: true }));
        let before = sup.view("/repo").unwrap();
        assert_eq!(before.status, PreviewStatus::Up);

        let opened = sup.open_view("/repo").expect("selected target");
        assert_eq!(opened, before, "open feedback keeps the observed target");
        assert_eq!(
            opened.source,
            thegn_core::preview::PortHintSource::PaneOutput
        );

        // The re-armed probe may downgrade the live projection later, but the
        // view returned to the open/copy action is untouched.
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Seen);
        let requests = sup.probe_requests("/repo");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].port, 8765);
    }

    #[test]
    fn simultaneous_worktrees_keep_independent_targets_and_probe_state() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(
            41,
            "/alpha",
            Some("alpha-session".into()),
            b"  ready http://localhost:5173/\n",
            Some(80)
        ));
        assert!(sup.pane_output(
            42,
            "/beta",
            Some("beta-session".into()),
            b"  Local: http://127.0.0.1:3000/\n",
            Some(80)
        ));

        let alpha = sup.view("/alpha").unwrap();
        let beta = sup.view("/beta").unwrap();
        assert_eq!((alpha.port, alpha.status), (5173, PreviewStatus::Seen));
        assert_eq!((beta.port, beta.status), (3000, PreviewStatus::Seen));
        assert_eq!(alpha.url, "http://localhost:5173/");
        assert_eq!(beta.url, "http://127.0.0.1:3000/");

        let alpha_request = sup.probe_requests("/alpha");
        assert_eq!(alpha_request.len(), 1);
        assert_eq!(alpha_request[0].worktree, "/alpha");
        assert_eq!(alpha_request[0].port, 5173);
        assert!(sup.apply_probe(ProbeResult {
            request: alpha_request[0].clone(),
            ok: true,
        }));

        // Switching the active worktree rearms only that worktree's targets.
        let beta_request = sup.probe_requests("/beta");
        assert_eq!(beta_request.len(), 1);
        assert_eq!(beta_request[0].worktree, "/beta");
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Up);
        assert_eq!(sup.view("/beta").unwrap().status, PreviewStatus::Seen);

        // A beta pane exit stops beta; alpha's proof is untouched.
        assert!(sup.pane_exit(42));
        assert_eq!(sup.view("/beta").unwrap().status, PreviewStatus::Down);
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Up);
        assert_eq!(sup.view("/alpha").unwrap().port, 5173);
    }

    #[test]
    fn restart_moves_the_selected_target_from_stopped_to_new_port() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        sup.pane_output(51, "/repo", None, b"http://localhost:5173/\n", Some(80));
        let request = sup.probe_requests("/repo").pop().unwrap();
        assert!(sup.apply_probe(ProbeResult { request, ok: true }));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Up);

        assert!(sup.pane_exit(51));
        let stopped = sup.view("/repo").unwrap();
        assert_eq!((stopped.port, stopped.status), (5173, PreviewStatus::Down));

        // The restarted dev server prints a different port in a new pane. The
        // stopped target remains visible as history, but selection moves to
        // the new live target.
        assert!(sup.pane_output(52, "/repo", None, b"http://localhost:3000/\n", Some(80)));
        let restarted = sup.view("/repo").unwrap();
        assert_eq!(restarted.port, 3000);
        assert_eq!(restarted.status, PreviewStatus::Seen);
        assert_eq!(
            restarted.source,
            thegn_core::preview::PortHintSource::PaneOutput
        );

        let request = sup.probe_requests("/repo").pop().unwrap();
        assert_eq!(request.port, 3000);
        assert!(sup.apply_probe(ProbeResult { request, ok: true }));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Up);
    }

    /// What `preview_watch::spawn_probe` checks: a real loopback connect.
    async fn real_probe(request: ProbeRequest) -> ProbeResult {
        let ok = within(async {
            tokio::net::TcpStream::connect((request.host.as_str(), request.port))
                .await
                .is_ok()
        })
        .await;
        ProbeResult { request, ok }
    }

    async fn within<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(5), future)
            .await
            .expect("fixture operation timed out")
    }

    /// A minimal unmasked server-to-client text frame. The fixture's HMR peer
    /// only ever speaks this shape, which is enough to prove the browser keeps
    /// its own socket without adding a WebSocket dependency to the host crate.
    fn ws_text_frame(payload: &str) -> Vec<u8> {
        let bytes = payload.as_bytes();
        let mut frame = Vec::with_capacity(bytes.len() + 10);
        frame.push(0x81);
        match bytes.len() {
            0..=125 => frame.push(bytes.len() as u8),
            126..=65535 => {
                frame.push(126);
                frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            }
            _ => {
                frame.push(127);
                frame.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(bytes);
        frame
    }

    async fn read_http_response(
        reader: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    ) -> String {
        let mut response = Vec::new();
        let mut byte = [0_u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            let read = reader.read(&mut byte).await.expect("read handshake");
            if read == 0 {
                break;
            }
            response.push(byte[0]);
        }
        String::from_utf8(response).expect("utf8 handshake")
    }

    async fn ws_connect(addr: std::net::SocketAddr) -> tokio::io::BufReader<tokio::net::TcpStream> {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect hmr");
        let request = format!(
            "GET /hmr HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("hmr request");
        let mut reader = tokio::io::BufReader::new(stream);
        let response = read_http_response(&mut reader).await;
        assert!(
            response.starts_with("HTTP/1.1 101"),
            "handshake: {response}"
        );
        reader
    }

    async fn read_ws_text(reader: &mut tokio::io::BufReader<tokio::net::TcpStream>) -> String {
        let mut header = [0_u8; 2];
        reader.read_exact(&mut header).await.expect("ws header");
        assert_eq!(header[0] & 0x0f, 1, "expected a text frame");
        assert_eq!(header[1] & 0x80, 0, "server frames are unmasked");
        let len = match header[1] & 0x7f {
            126 => {
                let mut bytes = [0_u8; 2];
                reader.read_exact(&mut bytes).await.expect("ws length");
                u16::from_be_bytes(bytes) as usize
            }
            127 => {
                let mut bytes = [0_u8; 8];
                reader.read_exact(&mut bytes).await.expect("ws length");
                usize::try_from(u64::from_be_bytes(bytes)).expect("ws length fits")
            }
            len => usize::from(len),
        };
        let mut payload = vec![0_u8; len];
        reader.read_exact(&mut payload).await.expect("ws payload");
        String::from_utf8(payload).expect("utf8 ws text")
    }

    /// A loopback dev-server fixture with a page, an independently served HMR
    /// WebSocket, and an on-disk entry file. Editing the file updates both what
    /// the app serves and the HMR payload, like a real dev server's transform
    /// pipeline.
    struct DevFixture {
        _dir: tempfile::TempDir,
        index: std::path::PathBuf,
        app_addr: std::net::SocketAddr,
        hmr_addr: std::net::SocketAddr,
        content: Arc<Mutex<String>>,
        updates: tokio::sync::broadcast::Sender<String>,
        app_task: tokio::task::JoinHandle<()>,
        hmr_task: tokio::task::JoinHandle<()>,
    }

    impl DevFixture {
        async fn start(initial: &str) -> Self {
            let dir = tempfile::tempdir().expect("fixture dir");
            let index = dir.path().join("index.html");
            std::fs::write(&index, initial).expect("write fixture");
            let content = Arc::new(Mutex::new(initial.to_string()));

            let hmr_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind hmr");
            let hmr_addr = hmr_listener.local_addr().expect("hmr addr");
            let (updates, _) = tokio::sync::broadcast::channel::<String>(8);
            let hmr_updates = updates.clone();
            let hmr_task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = hmr_listener.accept().await else {
                        break;
                    };
                    let updates = hmr_updates.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 512];
                        while !request
                            .windows(4)
                            .any(|window| window == b"\r\n\r\n".as_slice())
                        {
                            let Ok(read) = stream.read(&mut buffer).await else {
                                return;
                            };
                            if read == 0 || request.len() > 8192 {
                                return;
                            }
                            request.extend_from_slice(&buffer[..read]);
                        }
                        // Fixed RFC 6455 example key/accept pair; the fixture's
                        // client sends this key and does not import a browser
                        // identity.
                        let handshake = concat!(
                            "HTTP/1.1 101 Switching Protocols\r\n",
                            "Upgrade: websocket\r\n",
                            "Connection: Upgrade\r\n",
                            "Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
                        );
                        if stream.write_all(handshake.as_bytes()).await.is_err() {
                            return;
                        }
                        // Subscribe before announcing the connection so an edit
                        // made immediately after the client sees `connected`
                        // cannot race the receiver registration.
                        let mut rx = updates.subscribe();
                        if stream
                            .write_all(&ws_text_frame("{\"type\":\"connected\"}"))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        while let Ok(payload) = rx.recv().await {
                            if stream.write_all(&ws_text_frame(&payload)).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            });

            let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind app");
            let app_addr = app_listener.local_addr().expect("app addr");
            let page = format!(
                "<!doctype html><meta charset=\"utf-8\"><script>new WebSocket('ws://{hmr_addr}/')</script>"
            );
            let app_content = Arc::clone(&content);
            let app_task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = app_listener.accept().await else {
                        break;
                    };
                    let content = Arc::clone(&app_content);
                    let page = page.clone();
                    tokio::spawn(async move {
                        // Read the whole request head so closing never RSTs.
                        let mut request = Vec::new();
                        let mut chunk = [0_u8; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => request.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let body = content.lock().expect("fixture content").clone();
                        let document = format!("{page}<main>{body}</main>");
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{document}",
                            document.len()
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                        let _shutdown = stream.shutdown().await;
                    });
                }
            });

            Self {
                _dir: dir,
                index,
                app_addr,
                hmr_addr,
                content,
                updates,
                app_task,
                hmr_task,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/", self.app_addr)
        }

        /// Edit the fixture's entry file and publish the transformed body to
        /// connected HMR clients.
        fn edit(&self, marker: &str) -> String {
            let body = format!("<h1>{marker}</h1>");
            std::fs::write(&self.index, &body).expect("edit fixture");
            *self.content.lock().expect("fixture content") = body.clone();
            let payload = serde_json::json!({
                "type": "update",
                "path": "index.html",
                "body": body,
            })
            .to_string();
            let _sent = self.updates.send(payload);
            body
        }

        async fn served_page(&self) -> String {
            within(async {
                let mut stream = tokio::net::TcpStream::connect(self.app_addr)
                    .await
                    .expect("connect app");
                stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("request app");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .await
                    .expect("read app");
                response
            })
            .await
        }
    }

    impl DevFixture {
        /// Stop both listeners and wait until they are really closed, so the
        /// ports refuse connections afterwards.
        async fn stop(&mut self) {
            self.app_task.abort();
            self.hmr_task.abort();
            let _app = (&mut self.app_task).await;
            let _hmr = (&mut self.hmr_task).await;
        }
    }

    impl Drop for DevFixture {
        fn drop(&mut self) {
            self.app_task.abort();
            self.hmr_task.abort();
        }
    }

    #[tokio::test]
    async fn live_app_targets_keep_independent_hmr_and_reopen_after_restart() {
        let mut alpha = DevFixture::start("<h1>alpha one</h1>").await;
        let beta = DevFixture::start("<h1>beta one</h1>").await;

        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(
            101,
            "/alpha",
            Some("alpha-session".into()),
            format!("  Local:   {}\n", alpha.url()).as_bytes(),
            Some(80),
        ));
        assert!(sup.pane_output(
            102,
            "/beta",
            Some("beta-session".into()),
            format!("  Local:   {}\n", beta.url()).as_bytes(),
            Some(80),
        ));
        assert_eq!(sup.view("/alpha").unwrap().url, alpha.url());
        assert_eq!(sup.view("/beta").unwrap().url, beta.url());
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Seen);
        assert_eq!(sup.view("/beta").unwrap().status, PreviewStatus::Seen);

        let request = sup.probe_requests("/alpha").pop().expect("alpha probe");
        let alpha_request = request.clone();
        let result = real_probe(request).await;
        assert!(result.ok, "fixture answers a real loopback connect");
        assert!(sup.apply_probe(result));
        let request = sup.probe_requests("/beta").pop().expect("beta probe");
        let result = real_probe(request).await;
        assert!(result.ok, "fixture answers a real loopback connect");
        assert!(sup.apply_probe(result));
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Up);
        assert_eq!(sup.view("/beta").unwrap().status, PreviewStatus::Up);

        let mut alpha_ws = within(ws_connect(alpha.hmr_addr)).await;
        let hello = within(read_ws_text(&mut alpha_ws)).await;
        assert!(hello.contains("connected"));
        let mut beta_ws = within(ws_connect(beta.hmr_addr)).await;
        let hello = within(read_ws_text(&mut beta_ws)).await;
        assert!(hello.contains("connected"));

        // Editing alpha is visible over its HMR socket and on its app page;
        // beta keeps serving its own content.
        alpha.edit("alpha two");
        let update = within(read_ws_text(&mut alpha_ws)).await;
        assert!(update.contains("alpha two"));
        assert!(update.contains("index.html"));
        assert!(alpha.served_page().await.contains("alpha two"));
        assert!(!alpha.served_page().await.contains("alpha one"));
        assert!(beta.served_page().await.contains("beta one"));

        beta.edit("beta two");
        let update = within(read_ws_text(&mut beta_ws)).await;
        assert!(update.contains("beta two"));
        assert!(!update.contains("alpha two"));
        assert_eq!(sup.view("/alpha").unwrap().url, alpha.url());

        // Opening snapshots the current target (including its `up` status)
        // before re-arming a fresh probe for later feedback.
        let before_open = sup.view("/alpha").unwrap();
        let opened = sup.open_view("/alpha").expect("open target");
        assert_eq!(opened, before_open);
        assert_eq!(
            thegn_core::url_launch::normalize_url(&opened.url),
            Ok(opened.url.clone())
        );
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Seen);
        let request = sup.probe_requests("/alpha").pop().expect("re-probe");
        let result = real_probe(request).await;
        assert!(result.ok, "fixture answers a real loopback connect");
        assert!(sup.apply_probe(result));

        // Stop alpha, then bring a replacement dev server up on a new port.
        let old_url = alpha.url();
        alpha.stop().await;
        let stale = real_probe(alpha_request).await;
        assert!(!stale.ok, "the stopped server's port refuses connections");
        assert!(sup.pane_exit(101));
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Down);
        assert_eq!(sup.view("/alpha").unwrap().url, old_url);

        let restarted = DevFixture::start("<h1>alpha restarted</h1>").await;
        assert!(sup.pane_output(
            202,
            "/alpha",
            Some("alpha-restarted".into()),
            format!("  Local:   {}\n", restarted.url()).as_bytes(),
            Some(80),
        ));
        let view = sup.view("/alpha").unwrap();
        assert_eq!(view.port, restarted.app_addr.port());
        assert_eq!(view.status, PreviewStatus::Seen);
        assert_ne!(view.url, old_url);
        assert_eq!(sup.view("/beta").unwrap().url, beta.url());

        let request = sup.probe_requests("/alpha").pop().expect("restarted probe");
        let result = real_probe(request).await;
        assert!(result.ok, "fixture answers a real loopback connect");
        assert!(sup.apply_probe(result));
        assert_eq!(sup.view("/alpha").unwrap().status, PreviewStatus::Up);
        assert!(restarted.served_page().await.contains("alpha restarted"));
    }

    fn pane_ports(sup: &PreviewSupervisor) -> Vec<u16> {
        sup.candidates
            .iter()
            .filter(|(_, candidate)| !candidate.panes.is_empty())
            .map(|((_, port), _)| *port)
            .collect::<Vec<_>>()
    }

    #[test]
    fn url_split_across_two_chunks_is_found() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(!sup.pane_output(3, "/repo", None, b"server at http://local", Some(80)));
        assert!(sup.pane_output(3, "/repo", None, b"host:5173/ ready\n", Some(80)));
        assert_eq!(pane_ports(&sup), vec![5173]);
    }

    #[test]
    fn url_split_across_a_soft_wrap_is_found_with_a_full_tail() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        // Fill the tail so the windowed parse (not the whole tail) is in play.
        let mut filler = Vec::new();
        while filler.len() < DIAGNOSTIC_TAIL_BYTES {
            filler.extend_from_slice(b"compiling crate foo v1.2.3\r\n");
        }
        sup.pane_output(4, "/repo", None, &filler, Some(20));
        // A 20-column row that ends exactly at the margin, then a separate
        // chunk carrying the rest of the URL on the next visual row.
        assert!(!sup.pane_output(4, "/repo", None, b"http://localhost:876\r\n", Some(20)));
        assert!(sup.pane_output(4, "/repo", None, b"5/ ready\r\n", Some(20)));
        assert_eq!(pane_ports(&sup), vec![8765]);
    }

    #[test]
    fn port_digits_in_two_chunks_resolve_and_supersede_the_prefix() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        // A trailing port stays incomplete until terminated.
        assert!(!sup.pane_output(5, "/repo", None, b"http://localhost:87", Some(80)));
        assert!(sup.view("/repo").is_none());
        assert!(sup.pane_output(5, "/repo", None, b"65/\n", Some(80)));
        assert_eq!(pane_ports(&sup), vec![8765]);

        // An already-recorded prefix port is superseded by its extension.
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(6, "/repo", None, b"http://localhost:87 \n", Some(80)));
        assert_eq!(pane_ports(&sup), vec![87]);
        // While the prefix is still inside the parse window both are distinct
        // live ports, exactly as with a whole-tail parse; once it is older
        // than the window, the extension supersedes it.
        let logs = format!("{}\n", "w".repeat(60)).repeat(30);
        assert!(!sup.pane_output(6, "/repo", None, logs.as_bytes(), Some(80)));
        assert!(sup.pane_output(6, "/repo", None, b"http://localhost:8765/\n", Some(80)));
        assert_eq!(pane_ports(&sup), vec![8765]);
    }

    #[test]
    fn hint_older_than_the_window_is_retained() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(8, "/repo", None, b"http://localhost:5173/\n", Some(80)));
        for index in 0..200 {
            let line = format!("log line {index:04} {}\n", "y".repeat(60));
            sup.pane_output(8, "/repo", None, line.as_bytes(), Some(80));
        }
        assert!(sup.pane_tail_len(8) <= DIAGNOSTIC_TAIL_BYTES);
        assert_eq!(pane_ports(&sup), vec![5173]);
        assert_eq!(sup.view("/repo").unwrap().port, 5173);
    }

    #[test]
    fn chunk_without_hint_change_does_not_publish() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        assert!(sup.pane_output(10, "/repo", None, b"http://localhost:5173/\n", Some(80)));
        let published = sup.publishes.load(std::sync::atomic::Ordering::Relaxed);
        assert!(published >= 1);
        // Spinner frames with no hint text.
        for second in 0..50 {
            let frame = format!("\r\u{280b} building\u{2026} {second}s");
            assert!(!sup.pane_output(10, "/repo", None, frame.as_bytes(), Some(80)));
        }
        // A frame whose window still contains the unchanged URL is a no-op.
        assert!(!sup.pane_output(10, "/repo", None, b"\r", Some(80)));
        assert_eq!(
            sup.publishes.load(std::sync::atomic::Ordering::Relaxed),
            published
        );
        // The published diagnostic is the live tail, not a stale copy.
        assert!(sup.diagnostic("/repo").unwrap().contains("49s"));
    }

    #[test]
    fn tail_trim_is_amortized_but_the_view_stays_bounded() {
        let mut sup = PreviewSupervisor::new();
        sup.set_enabled(true);
        let chunk = vec![b'z'; 4096];
        for _ in 0..40 {
            sup.pane_output(11, "/repo", None, &chunk, Some(80));
            let retained = lock_tail(&sup.pane_tails[&11]).len();
            assert!(retained <= TAIL_TRIM_THRESHOLD);
            assert!(sup.pane_tail_len(11) <= DIAGNOSTIC_TAIL_BYTES);
        }
        assert_eq!(sup.pane_tail_len(11), DIAGNOSTIC_TAIL_BYTES);
    }

    #[test]
    fn parse_window_start_table() {
        // (tail, new_len, cols, expected start)
        let long_prefix = format!("{}\nab", "p".repeat(3000));
        let long_line = "q".repeat(MAX_LINE_LOOKBACK_BYTES + 100);
        // A multibyte char straddling the overlap cut on a long line.
        let wide_line = "\u{4e16}".repeat(MAX_LINE_LOOKBACK_BYTES);
        let cases: [(&[u8], usize, Option<u16>, usize); 8] = [
            // Empty tail and whole-tail chunks start at 0.
            (b"", 0, None, 0),
            (b"hello", 5, Some(80), 0),
            // Short history: the overlap reaches the tail start.
            (b"one\ntwo\nthree", 2, None, 0),
            (b"one\ntwo\nthree", 2, Some(80), 0),
            // Long history: start at the first line start inside the overlap
            // before the new byte's line (`p...p\n` is 3001 bytes; the line
            // holding `b` starts at 3001; the overlap cut is 1977 and the first
            // line start at/after it is 3001 itself).
            (long_prefix.as_bytes(), 1, None, 3001),
            (long_prefix.as_bytes(), 1, Some(1600), 0),
            // An unbroken line longer than the look-back starts one overlap
            // before the new bytes.
            (long_line.as_bytes(), 10, None, long_line.len() - 10 - 1024),
            (
                long_line.as_bytes(),
                10,
                Some(1000),
                long_line.len() - 10 - 2000,
            ),
        ];
        for (tail, new_len, cols, expected) in cases {
            assert_eq!(
                parse_window_start(tail, new_len, cols),
                expected,
                "tail len {} new {new_len} cols {cols:?}",
                tail.len()
            );
        }
        // Char boundary: the cut lands mid-char and moves forward to the next
        // char start.
        let wide = wide_line.as_bytes();
        let start = parse_window_start(wide, 3, None);
        assert!(wide_line.is_char_boundary(start));
        assert!(start >= wide.len() - 3 - 1024);
        assert!(start <= wide.len() - 3 - 1024 + 2);
        // A new byte that is itself a line break keeps the previous line (a
        // trailing port may still be incomplete there).
        let tail = format!("{}\nhttp://localhost:87\n", "r".repeat(4000));
        assert_eq!(parse_window_start(tail.as_bytes(), 1, None), 4001);
        // The window never exceeds the diagnostic bound.
        let huge = vec![b'x'; TAIL_TRIM_THRESHOLD];
        assert_eq!(
            parse_window_start(&huge, huge.len(), None),
            huge.len() - DIAGNOSTIC_TAIL_BYTES
        );
    }

    #[test]
    fn ceil_char_boundary_skips_continuation_bytes_only() {
        let text = "a\u{4e16}b".as_bytes();
        assert_eq!(ceil_char_boundary(text, 0), 0);
        assert_eq!(ceil_char_boundary(text, 2), 4);
        assert_eq!(ceil_char_boundary(text, 3), 4);
        assert_eq!(ceil_char_boundary(text, 4), 4);
        assert_eq!(ceil_char_boundary(text, 99), text.len());
        // Invalid runs of continuation bytes are bounded to three skips.
        assert_eq!(ceil_char_boundary(&[0x80; 8], 0), 3);
    }
}
