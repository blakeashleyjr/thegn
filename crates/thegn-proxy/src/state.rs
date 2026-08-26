//! The daemon's shared state, handed to every axum handler behind an `Arc`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use thegn_core::proxy::ratelimit::{InflightTracker, RateLimiter};

use crate::health::Health;
use crate::metrics::Metrics;
use crate::model::ProxyConfig;
use crate::shared::SharedDb;
use crate::upstream::Upstreams;

/// Process-wide proxy state. Cheap to clone (everything is `Arc`/shared).
pub struct AppState {
    pub config: ProxyConfig,
    pub health: Arc<Health>,
    pub limiter: Arc<RateLimiter>,
    pub inflight: Arc<InflightTracker>,
    pub metrics: Arc<Metrics>,
    pub client: reqwest::Client,
    /// The upstream dispatch seam (adapters keyed by wire protocol).
    pub upstreams: Upstreams,
    pub db: SharedDb,
    /// Route name → identity of the backend that last served it (`/resolved`).
    resolved: Mutex<HashMap<String, String>>,
    /// Latest `[usage]` account-headroom snapshot for usage-aware ordering:
    /// provider name → peak used percent (0–100). Empty unless `usage_aware`.
    usage_snapshot: RwLock<HashMap<String, f32>>,
    /// Daemon start (epoch ms) — `/metrics` uptime + `/stats`.
    pub started_ms: i64,
}

/// Handlers receive `State<SharedState>`.
pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(config: ProxyConfig, db: SharedDb, now_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            config,
            health: Arc::new(Health::new()),
            limiter: Arc::new(RateLimiter::new()),
            inflight: Arc::new(InflightTracker::new()),
            metrics: Arc::new(Metrics::new()),
            client: reqwest::Client::new(),
            upstreams: Upstreams::new(),
            db,
            resolved: Mutex::new(HashMap::new()),
            usage_snapshot: RwLock::new(HashMap::new()),
            started_ms: now_ms,
        })
    }

    pub fn set_resolved(&self, route: &str, backend_identity: &str) {
        self.resolved
            .lock()
            .unwrap()
            .insert(route.to_string(), backend_identity.to_string());
    }

    pub fn resolved_snapshot(&self) -> HashMap<String, String> {
        self.resolved.lock().unwrap().clone()
    }

    /// Replaces the usage-headroom snapshot (called by the background refresher).
    pub fn set_usage_snapshot(&self, snapshot: HashMap<String, f32>) {
        if let Ok(mut g) = self.usage_snapshot.write() {
            *g = snapshot;
        }
    }

    /// Peak used percent for a provider account, if the snapshot knows it.
    pub fn provider_used_percent(&self, provider: &str) -> Option<f32> {
        self.usage_snapshot
            .read()
            .ok()
            .and_then(|g| g.get(provider).copied())
    }
}
