//! Backend health tracking: an in-memory exhaustion map + per-class backoff
//! (from `thegn_core::backoff`).
//!
//! Health is **in-memory only** — a daemon restart clears cooldowns and lanes
//! are re-probed on the next request. (The resurrection's two DB tables are
//! accounting-only; no `model_proxy_health` table exists, by design.) The
//! Claude-Max credential-file gating from the pre-alpha proxy is not restored.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use thegn_core::backoff::{backoff_config_for, backoff_from_config, classify_exhaustion};

/// An in-memory exhaustion marker.
#[derive(Clone, Debug)]
struct Marker {
    reason: String,
    next_probe_ms: i64,
    consecutive_failures: i64,
}

/// Tracks which backends are cooled down and until when.
#[derive(Default)]
pub struct Health {
    markers: Mutex<HashMap<String, Marker>>,
}

fn key(backend: &str, model: &str) -> String {
    format!("{backend}:{model}")
}

impl Health {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `(backend, model)` is currently cooled down at `now_ms`.
    pub fn is_exhausted(&self, backend: &str, model: &str, now_ms: i64) -> bool {
        let markers = self.markers.lock().unwrap();
        match markers.get(&key(backend, model)) {
            Some(m) => now_ms < m.next_probe_ms,
            None => false,
        }
    }

    /// Marks a backend exhausted, classifying the reason and applying the
    /// per-class backoff. When `until_ms` is given (a precise upstream reset), it
    /// overrides the computed cooldown.
    pub fn mark_exhausted(
        &self,
        backend: &str,
        model: &str,
        reason: &str,
        until_ms: Option<i64>,
        now_ms: i64,
    ) {
        let kind = classify_exhaustion(reason, 0);
        let mut markers = self.markers.lock().unwrap();
        let k = key(backend, model);
        let consecutive = markers
            .get(&k)
            .map(|m| m.consecutive_failures + 1)
            .unwrap_or(0);
        let next_probe_ms = match until_ms {
            Some(t) => t,
            None => {
                let backoff = backoff_from_config(backoff_config_for(kind), consecutive as u32);
                now_ms + backoff.as_millis() as i64
            }
        };
        markers.insert(
            k,
            Marker {
                reason: reason.to_string(),
                next_probe_ms,
                consecutive_failures: consecutive,
            },
        );
    }

    /// Briefly parks a backend after a stream-path soft failure (TTFB timeout /
    /// empty completion). Short, escalating, never permanent.
    pub fn mark_soft_cooldown(
        &self,
        backend: &str,
        model: &str,
        reason: &str,
        base: Duration,
        now_ms: i64,
    ) {
        if base.is_zero() {
            return;
        }
        let mut markers = self.markers.lock().unwrap();
        let k = key(backend, model);
        let consecutive = markers
            .get(&k)
            .map(|m| m.consecutive_failures + 1)
            .unwrap_or(0);
        let cfg = thegn_core::backoff::BackoffConfig {
            initial: base,
            multiplier: 2.0,
            ceiling: base * 8,
            jitter: 0.2,
        };
        let backoff = backoff_from_config(cfg, consecutive as u32);
        markers.insert(
            k,
            Marker {
                reason: reason.to_string(),
                next_probe_ms: now_ms + backoff.as_millis() as i64,
                consecutive_failures: consecutive,
            },
        );
    }

    /// Clears the marker and failure counter on a successful request.
    pub fn record_success(&self, backend: &str, model: &str) {
        self.markers.lock().unwrap().remove(&key(backend, model));
    }

    /// A snapshot of exhausted backends for the status endpoints:
    /// `(identity, reason, next_probe_ms, healthy_now)`.
    pub fn status(&self, now_ms: i64) -> Vec<(String, String, i64, bool)> {
        let markers = self.markers.lock().unwrap();
        markers
            .iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    m.reason.clone(),
                    m.next_probe_ms,
                    now_ms >= m.next_probe_ms,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_clear() {
        let h = Health::new();
        assert!(!h.is_exhausted("openrouter", "m", 1000));
        h.mark_exhausted("openrouter", "m", "HTTP 429 (rate limit)", None, 1000);
        assert!(h.is_exhausted("openrouter", "m", 1000));
        h.record_success("openrouter", "m");
        assert!(!h.is_exhausted("openrouter", "m", 1000));
    }

    #[test]
    fn until_overrides_cooldown() {
        let h = Health::new();
        h.mark_exhausted("p", "m", "HTTP 429 (rate limit)", Some(5_000), 0);
        assert!(h.is_exhausted("p", "m", 4_999));
        assert!(!h.is_exhausted("p", "m", 5_000));
    }

    #[test]
    fn soft_cooldown_parks_briefly() {
        let h = Health::new();
        h.mark_soft_cooldown("p", "m", "stream empty", Duration::from_millis(100), 0);
        assert!(h.is_exhausted("p", "m", 0));
        h.record_success("p", "m");
        h.mark_soft_cooldown("p", "m", "x", Duration::ZERO, 0);
        assert!(!h.is_exhausted("p", "m", 0));
    }

    #[test]
    fn status_reports_markers() {
        let h = Health::new();
        h.mark_exhausted("p", "m", "HTTP 402 (payment)", None, 0);
        let snap = h.status(0);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "p:m");
        assert!(!snap[0].3); // still cooling
    }
}
