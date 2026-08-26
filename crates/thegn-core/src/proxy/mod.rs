//! Pure, substrate-agnostic model-proxy logic.
//!
//! Everything here is synchronous and side-effect-free (no tokio, no network, no
//! env reads): it is the testable spine of the resurrected model proxy and is
//! subject to the core 95% coverage gate. The async I/O shell (the axum server,
//! reqwest streaming, the router loop, SQLite persistence) lives in the
//! `thegn-proxy` crate and composes over these types.
//!
//! Resurrected from the pre-alpha `thegn_core::proxy` (excised in `85f3d1fb`):
//! classification, cost estimation, credential pools, rate limiting, request
//! transforms, protocol translation, and stats rollups — minus the token
//! compression (`compress.rs`) and the sealed-egress bridge, which stay dead.
//! Two modules are new to the resurrection: the deterministic
//! [`route_select`](mod@route_select) `auto` tier classifier and
//! [`usage_order`](mod@usage_order) usage-aware lane ordering.

pub mod attribution;
pub mod classify;
pub mod cost;
pub mod creds;
pub mod ratelimit;
pub mod route_select;
pub mod stats;
pub mod transform;
pub mod translate;
pub mod usage_order;

pub use classify::{FailKind, classify_response, is_auth_exhaustion_reason};
pub use cost::{CostSource, PricePoint, PriceTable, Usage, cost_usd};
pub use creds::{CredPool, KeyStrategy, provider_base, split_keys};
pub use ratelimit::{InflightTracker, RateLimiter, RatePolicy, TokenBucket, parse_rate_policy};
pub use route_select::{AutoTier, RequestFeatures, route_select};
pub use usage_order::usage_order;
