//! Pure, substrate-agnostic LLM-proxy logic.
//!
//! This is the Rust port of the pure-logic half of the Go `model-proxy`
//! (`~/code/life-automation/apps/services/model-proxy/`). Everything here is
//! synchronous and side-effect-free (no tokio, no network, no env reads): it is
//! the testable spine of the proxy and is subject to the core 95% coverage gate.
//!
//! The async I/O shell (axum server, reqwest streaming, the router loop, SQLite
//! persistence) lives in the `thegn-proxy` crate and composes over these
//! types. Configuration (rate policies, key lists, provider routes) is resolved
//! by that crate from `[llm_proxy]` config + env overrides and passed in as
//! explicit parameters — core never reads the environment itself.

pub mod backoff;
pub mod bridge;
pub mod classify;
pub mod compress;
pub mod cost;
pub mod creds;
pub mod ratelimit;
pub mod stats;
pub mod transform;

pub use backoff::{BackoffConfig, ExhaustionKind, calculate_backoff};
pub use classify::{FailKind, classify_response, is_auth_exhaustion_reason};
pub use cost::{PriceTable, cost_usd};
pub use creds::{CredPool, KeyStrategy};
pub use ratelimit::{RateLimiter, TokenBucket};
