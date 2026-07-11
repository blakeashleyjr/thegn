//! Backend health tracking: the in-memory exhaustion map + per-class backoff
//! (from `thegn_core::proxy::backoff`), persisted to `proxy_health` so a
//! cooled-down backend survives a daemon restart. Port of `HealthTracker`.
//!
//! The Claude-Max credential-file gating (`MarkExhaustedAuth`) is intentionally
//! omitted in milestone 1 — the OAuth subscription path is deferred.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(test)]
use thegn_core::db::Db;
use thegn_core::proxy::backoff::{
    ExhaustionKind, backoff_config_for, backoff_from_config, classify_exhaustion,
};
use thegn_core::store::ProxyStore;

use crate::shared::SharedDb;

/// An in-memory exhaustion marker.
#[derive(Clone, Debug)]
struct Marker {
    kind: ExhaustionKind,
    reason: String,
    since_ms: i64,
    next_probe_ms: i64,
    is_stale: bool,
    consecutive_failures: i64,
}

/// Tracks which backends are cooled down and until when.
pub struct Health {
    markers: Mutex<HashMap<String, Marker>>,
    db: SharedDb,
}

fn key(backend: &str, model: &str) -> String {
    format!("{backend}:{model}")
}

fn kind_str(kind: ExhaustionKind) -> &'static str {
    match kind {
        ExhaustionKind::Unknown => "unknown",
        ExhaustionKind::RateLimit => "rate_limit",
        ExhaustionKind::Payment => "payment",
        ExhaustionKind::Auth => "auth",
        ExhaustionKind::ServerError => "server_error",
        ExhaustionKind::ClientError => "client_error",
    }
}

fn kind_from_str(s: &str) -> ExhaustionKind {
    match s {
        "rate_limit" => ExhaustionKind::RateLimit,
        "payment" => ExhaustionKind::Payment,
        "auth" => ExhaustionKind::Auth,
        "server_error" => ExhaustionKind::ServerError,
        "client_error" => ExhaustionKind::ClientError,
        _ => ExhaustionKind::Unknown,
    }
}

impl Health {
    /// Builds a tracker and hydrates live markers from the DB.
    pub fn new(db: SharedDb, now_ms: i64) -> Self {
        let mut markers = HashMap::new();
        if let Ok(guard) = db.lock()
            && let Ok(rows) = guard.load_proxy_health(now_ms)
        {
            for r in rows {
                markers.insert(
                    key(&r.backend, &r.model),
                    Marker {
                        kind: kind_from_str(&r.kind),
                        reason: r.reason,
                        since_ms: r.since_ms,
                        next_probe_ms: r.next_probe_ms,
                        is_stale: r.is_stale,
                        consecutive_failures: r.consecutive_failures,
                    },
                );
            }
        }
        Self {
            markers: Mutex::new(markers),
            db,
        }
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
    /// per-class backoff. When `until_ms` is given (a precise upstream reset),
    /// it overrides the computed cooldown. Persists the marker.
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
        let is_stale = until_ms.is_none() && kind.is_stale();
        let marker = Marker {
            kind,
            reason: reason.to_string(),
            since_ms: now_ms,
            next_probe_ms,
            is_stale,
            consecutive_failures: consecutive,
        };
        self.persist(backend, model, &marker);
        markers.insert(k, marker);
    }

    /// Briefly parks a backend after a stream-path soft failure (TTFB timeout /
    /// empty completion). Short, escalating, never permanent. Mirrors
    /// `MarkSoftCooldown`.
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
        let cfg = thegn_core::proxy::backoff::BackoffConfig {
            initial: base,
            multiplier: 2.0,
            ceiling: base * 8,
            jitter: 0.2,
        };
        let backoff = backoff_from_config(cfg, consecutive as u32);
        let marker = Marker {
            kind: ExhaustionKind::Unknown,
            reason: reason.to_string(),
            since_ms: now_ms,
            next_probe_ms: now_ms + backoff.as_millis() as i64,
            is_stale: false,
            consecutive_failures: consecutive,
        };
        self.persist(backend, model, &marker);
        markers.insert(k, marker);
    }

    /// Clears the marker and failure counter on a successful request.
    pub fn record_success(&self, backend: &str, model: &str) {
        let mut markers = self.markers.lock().unwrap();
        if markers.remove(&key(backend, model)).is_some()
            && let Ok(db) = self.db.lock()
        {
            let _ = db.clear_proxy_health(backend, model);
        }
    }

    /// A snapshot of exhausted backends for the `/resolved` / status endpoints:
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

    fn persist(&self, backend: &str, model: &str, m: &Marker) {
        if let Ok(db) = self.db.lock() {
            let _ = db.put_proxy_health(
                backend,
                model,
                kind_str(m.kind),
                &m.reason,
                m.since_ms,
                m.next_probe_ms,
                m.is_stale,
                m.consecutive_failures,
                None,
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn db() -> SharedDb {
        Arc::new(Mutex::new(Db::open_memory().unwrap()))
    }

    #[test]
    fn mark_exhausted_persists_and_reloads() {
        let db = db();
        let h = Health::new(db.clone(), 1000);
        assert!(!h.is_exhausted("openrouter", "m", 1000));
        // A rate-limit 429 cools the backend down (future next_probe).
        h.mark_exhausted("openrouter", "m", "HTTP 429 (rate limit)", None, 1000);
        assert!(h.is_exhausted("openrouter", "m", 1000));

        // A fresh tracker hydrates the live marker from the DB (covers new() +
        // kind_from_str reverse mapping).
        let h2 = Health::new(db, 1000);
        assert!(h2.is_exhausted("openrouter", "m", 1000));
    }

    #[test]
    fn mark_exhausted_until_overrides_cooldown() {
        let h = Health::new(db(), 0);
        h.mark_exhausted("p", "m", "HTTP 429 (rate limit)", Some(5_000), 0);
        assert!(h.is_exhausted("p", "m", 4_999));
        assert!(!h.is_exhausted("p", "m", 5_000));
    }

    #[test]
    fn record_success_clears_marker_and_db() {
        let db = db();
        let h = Health::new(db.clone(), 0);
        h.mark_exhausted("p", "m", "HTTP 500 (upstream outage)", None, 0);
        assert!(h.is_exhausted("p", "m", 0));
        h.record_success("p", "m");
        assert!(!h.is_exhausted("p", "m", 0));
        // Cleared in the DB too: a fresh tracker sees nothing.
        assert!(!Health::new(db, 0).is_exhausted("p", "m", 0));
    }

    #[test]
    fn soft_cooldown_parks_briefly_then_clears() {
        let h = Health::new(db(), 0);
        h.mark_soft_cooldown(
            "p",
            "m",
            "stream empty completion",
            Duration::from_millis(100),
            0,
        );
        assert!(h.is_exhausted("p", "m", 0));
        // Never permanent — a no-op base does nothing.
        h.record_success("p", "m");
        h.mark_soft_cooldown("p", "m", "x", Duration::ZERO, 0);
        assert!(!h.is_exhausted("p", "m", 0));
    }

    #[test]
    fn status_reports_markers() {
        let h = Health::new(db(), 0);
        h.mark_exhausted("p", "m", "HTTP 402 (payment)", None, 0);
        let snap = h.status(0);
        assert_eq!(snap.len(), 1);
        let (ident, reason, next_probe, healthy) = &snap[0];
        assert_eq!(ident, "p:m");
        assert!(reason.contains("payment"));
        assert!(*next_probe > 0);
        assert!(!*healthy); // still cooling at now=0
    }
}
