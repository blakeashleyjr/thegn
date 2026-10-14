//! Loop-local supervisor for live browser-preview targets.
//!
//! Discovery facts arrive from existing event sources: bounded pane output,
//! pane exit/EOF, one-shot config/package scans, and sandbox forward events.
//! The supervisor is memory-only; forward DB rows remain the forwarder's cache,
//! never a second preview source of truth.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use thegn_core::preview::{
    PortHint, PortHintSource, PreviewStatus, PreviewTarget, parse_pane_port_hints_wrapped,
    select_target,
};

use crate::chrome::PreviewView;

/// Pane diagnostic bytes retained per pane. This matches the pure parser's
/// maximum input so repeated output can never grow host memory without bound.
const DIAGNOSTIC_TAIL_BYTES: usize = thegn_core::preview::MAX_PORT_HINT_CHARS;

/// Same-process response snapshot. A detached daemon legitimately has no
/// compositor-side pane diagnostics and therefore leaves this empty.
static DIAGNOSTIC_SNAPSHOTS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub(crate) fn diagnostic_snapshot(worktree: &str) -> Option<String> {
    DIAGNOSTIC_SNAPSHOTS
        .get()
        .and_then(|snapshots| snapshots.lock().ok()?.get(worktree).cloned())
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
    pane_tails: HashMap<u32, Vec<u8>>,
    pane_strippers: HashMap<u32, thegn_core::history::AnsiStripper>,
    pane_worktrees: HashMap<u32, String>,
    latest_scan_generation: u64,
    diagnostics: HashMap<String, String>,
    last_probe_worktree: Option<String>,
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

    /// Feed one pane's bounded output tail to the pure core parser. Pane facts
    /// persist after their URL scrolls out of the tail and are removed only
    /// when a longer digit-prefix hint from that pane supersedes them. Returns
    /// true only when the selected render projection changed.
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
        let before = self.view(worktree);
        // Strip incrementally before retaining the bounded tail. Keeping the
        // stripper state per pane matters when an OSC/DCS sequence spans chunks
        // or exceeds the tail bound: once the opener is evicted, hidden payload
        // bytes must not become parser-visible plain text.
        let mut visible = Vec::with_capacity(bytes.len());
        self.pane_strippers
            .entry(pane_id)
            .or_default()
            .strip_into(bytes, &mut visible);
        let tail = self.pane_tails.entry(pane_id).or_default();
        tail.extend_from_slice(&visible);
        if tail.len() > DIAGNOSTIC_TAIL_BYTES {
            tail.drain(..tail.len() - DIAGNOSTIC_TAIL_BYTES);
        }
        self.pane_worktrees.insert(pane_id, worktree.to_string());
        let text = String::from_utf8_lossy(tail);
        let hints = parse_pane_port_hints_wrapped(&text, cols);
        let new_ports: std::collections::BTreeSet<u16> =
            hints.iter().map(|hint| hint.port).collect();
        let old_ports: Vec<u16> = self
            .candidates
            .iter()
            .filter(|((candidate_worktree, _), candidate)| {
                candidate_worktree == worktree && candidate.panes.contains_key(&pane_id)
            })
            .map(|((_, port), _)| *port)
            .collect();
        // Keep pane facts after their URL scrolls out of the bounded tail. A
        // new hint only supersedes an old one when it extends that old port's
        // digits (for example, an echoed `:87` becoming `:8765`).
        for port in old_ports.into_iter().filter(|old| {
            !new_ports.contains(old)
                && new_ports
                    .iter()
                    .any(|new| new != old && new.to_string().starts_with(&old.to_string()))
        }) {
            if let Some(candidate) = self.candidates.get_mut(&(worktree.to_string(), port)) {
                candidate.panes.remove(&pane_id);
                Self::rearm(candidate);
            }
        }
        for hint in hints {
            let candidate = self
                .candidates
                .entry((worktree.to_string(), hint.port))
                .or_default();
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
        let pane_tail = self.view(worktree).and_then(|view| {
            self.candidates
                .get(&(worktree.to_string(), view.port))
                .and_then(|candidate| candidate.panes.keys().next())
                .and_then(|pane| self.pane_tails.get(pane))
                .map(|tail| String::from_utf8_lossy(tail).into_owned())
        });
        pane_tail.or_else(|| self.diagnostics.get(worktree).cloned())
    }

    fn publish_diagnostic(&self, worktree: &str) {
        let snapshots = DIAGNOSTIC_SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()));
        let Ok(mut snapshots) = snapshots.lock() else {
            return;
        };
        match self.diagnostic(worktree) {
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
        assert!(sup.pane_tails[&7].len() <= DIAGNOSTIC_TAIL_BYTES);
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
        assert!(!probe_allowed(
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
}
