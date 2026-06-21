//! The daemon's shared state, handed to every axum handler behind an `Arc`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use superzej_core::proxy::cost::PriceTable;
use superzej_core::proxy::ratelimit::{InflightTracker, RateLimiter};

use crate::health::Health;
use crate::metrics::Metrics;
use crate::model::ProxyConfig;
use crate::shared::SharedDb;

/// Process-wide proxy state. Cheap to clone (everything is `Arc`/shared).
pub struct AppState {
    pub config: ProxyConfig,
    pub health: Arc<Health>,
    pub limiter: Arc<RateLimiter>,
    pub inflight: Arc<InflightTracker>,
    pub metrics: Arc<Metrics>,
    pub client: reqwest::Client,
    pub price_table: PriceTable,
    pub db: SharedDb,
    /// Route name → identity of the backend that last served it (`/resolved`).
    resolved: Mutex<HashMap<String, String>>,
    /// Whether a budget breach refuses (true) or downgrades (false).
    pub refuse_on_breach: bool,
}

/// Handlers receive `State<SharedState>`.
pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(config: ProxyConfig, db: SharedDb, now_ms: i64) -> Arc<Self> {
        let health = Arc::new(Health::new(db.clone(), now_ms));
        Arc::new(Self {
            config,
            health,
            limiter: Arc::new(RateLimiter::new()),
            inflight: Arc::new(InflightTracker::new()),
            metrics: Arc::new(Metrics::new()),
            client: reqwest::Client::new(),
            price_table: PriceTable::with_defaults(),
            db,
            resolved: Mutex::new(HashMap::new()),
            refuse_on_breach: true,
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
}
