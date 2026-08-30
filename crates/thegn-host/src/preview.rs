//! Loop-local supervisor for live browser-preview targets.
//!
//! Discovery facts arrive from existing event sources: bounded pane output,
//! pane exit/EOF, one-shot config/package scans, and sandbox forward events.
//! The supervisor is memory-only; forward DB rows remain the forwarder's cache,
//! never a second preview source of truth.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use thegn_core::preview::{
    PortHint, PreviewStatus, PreviewTarget, parse_pane_port_hints, select_target,
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
    panes: BTreeMap<u32, (PortHint, Option<String>)>,
    ended_panes: BTreeMap<u32, (PortHint, Option<String>)>,
    forwarded_url: Option<String>,
    provider_ended: bool,
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
        } else if let Some(hint) = &self.package {
            (hint, None, None)
        } else {
            return None;
        };
        let status = if self.forwarded_url.is_some() || !self.panes.is_empty() {
            PreviewStatus::Up
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
            && self.panes.is_empty()
            && self.ended_panes.is_empty()
            && self.forwarded_url.is_none()
            && !self.provider_ended
    }
}

/// Owns preview target lifecycle and bounded diagnostic tails for this UI
/// process. All mutation happens on the event loop.
#[derive(Default)]
pub(crate) struct PreviewSupervisor {
    enabled: bool,
    candidates: BTreeMap<(String, u16), Candidate>,
    pane_tails: HashMap<u32, Vec<u8>>,
    pane_worktrees: HashMap<u32, String>,
    latest_scan_generation: u64,
    diagnostics: HashMap<String, String>,
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
                candidate.configured = None;
                candidate.package = None;
            }
        }
        for hint in scan.configured {
            let port = hint.port;
            self.candidates
                .entry((scan.worktree.clone(), port))
                .or_default()
                .configured = Some(hint);
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

    /// Feed one pane's bounded output tail to the pure core parser. Returns
    /// true only when the selected render projection changed.
    pub(crate) fn pane_output(
        &mut self,
        pane_id: u32,
        worktree: &str,
        session: Option<String>,
        bytes: &[u8],
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let before = self.view(worktree);
        let tail = self.pane_tails.entry(pane_id).or_default();
        tail.extend_from_slice(bytes);
        if tail.len() > DIAGNOSTIC_TAIL_BYTES {
            tail.drain(..tail.len() - DIAGNOSTIC_TAIL_BYTES);
        }
        self.pane_worktrees.insert(pane_id, worktree.to_string());
        let text = String::from_utf8_lossy(tail);
        for hint in parse_pane_port_hints(&text) {
            let candidate = self
                .candidates
                .entry((worktree.to_string(), hint.port))
                .or_default();
            // A new live watcher supersedes ended evidence for this target;
            // keep exit history bounded by the concurrently-observed panes.
            candidate.ended_panes.clear();
            candidate.panes.insert(pane_id, (hint, session.clone()));
        }
        self.publish_diagnostic(worktree);
        before != self.view(worktree)
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
        for ((candidate_worktree, _), candidate) in &mut self.candidates {
            if candidate_worktree == &worktree
                && let Some(fact) = candidate.panes.remove(&pane_id)
            {
                candidate.ended_panes.insert(pane_id, fact);
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
            b"ready http://localhost:5173"
        ));
        let view = sup.view("/repo").unwrap();
        assert_eq!(view.port, 5173);
        assert_eq!(view.status, PreviewStatus::Up);
        assert_eq!(view.source, thegn_core::preview::PortHintSource::PaneOutput);
        assert!(sup.pane_tails[&7].len() <= DIAGNOSTIC_TAIL_BYTES);
        assert!(sup.diagnostic("/repo").unwrap().contains("localhost:5173"));

        assert!(sup.pane_exit(7));
        assert_eq!(sup.view("/repo").unwrap().status, PreviewStatus::Down);
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
}
