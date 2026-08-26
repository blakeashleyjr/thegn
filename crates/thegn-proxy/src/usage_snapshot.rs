//! Usage-headroom snapshots for usage-aware ordering.
//!
//! The design is emphatic that the proxy **never fetches quota itself** — it
//! consumes snapshots the shell already gathers (`[usage]` tracker). So rather
//! than reach into the usage tables, the daemon exposes a loopback `POST /usage`
//! endpoint the host pushes to on its usage-refresh cadence: a `provider → peak
//! used percent` map. When `usage_aware` is off the map stays empty and lane
//! ordering is unaffected.

use std::collections::HashMap;

use serde::Deserialize;

/// The push payload: provider name → peak used percent (0–100).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsagePush {
    #[serde(default)]
    pub providers: HashMap<String, f32>,
}
