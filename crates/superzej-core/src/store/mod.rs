//! Backend-agnostic repository traits — the **store seam**.
//!
//! superzej's local state lives in an embedded SQLite database
//! ([`crate::db::Db`], implemented over rusqlite). These traits factor that DB
//! API surface into cohesive, domain-scoped seams so that:
//!
//!   * a future **server-side / multi-user** superzej can supply a
//!     Postgres-backed implementation (e.g. via diesel) *without* the
//!     single-user shell taking on any Postgres/async weight, and
//!   * a future embedded-engine swap (e.g. turso, once it ships a production
//!     release) is a localized new `impl`, not a scattered rewrite.
//!
//! The seam is **sync** on purpose — `superzej-core` deliberately carries no
//! tokio; the DB is accessed off the event loop via `spawn_blocking`. A server
//! backend may be async on its own side but must not push async into these
//! traits.
//!
//! Each domain's methods live in a sibling `impl <Trait> for Db` module
//! (`db_*.rs` / `host_db.rs`) rather than in `db.rs`, and every consumer depends
//! on the trait (`&dyn WorkspaceStore` / `&impl WorkspaceStore`) rather than the
//! concrete `Db`, so a future backend that implements these traits drops in with
//! no consumer changes. Relocating the whole DB API surface this way took
//! `db.rs` from ~5200 lines to ≈3000.
//!
//! Ported domains (the full surface): [`WorkspaceStore`] (repos/worktrees/
//! session/UI/folders/layouts/env/pins/terminals), [`CacheStore`] (TTL caches),
//! [`AccountStore`], [`NotificationStore`] (feed + agent dispatch),
//! [`WorktreeAuxStore`] (registers/shares/forwards/merge-queue/disk/undo/audit),
//! [`PoolStore`] (warm-spare pool), [`HostStore`] (host state machine), and
//! [`ProxyStore`] (the `superzej-proxy` daemon's state).

mod account;
mod aux;
mod cache;
mod compute;
mod control;
mod hibernate;
mod host;
mod intent;
mod notification;
mod placement;
mod pool;
mod proxy;
mod trust;
mod workspace;
mod zone;

pub use account::AccountStore;
pub use aux::WorktreeAuxStore;
pub use cache::CacheStore;
pub use compute::{ComputeBudgetRow, ComputeLedgerStore, ComputeMeterRow};
pub use control::{ControlStore, DaemonRow, LeaseRow, PairingRow};
pub use hibernate::{HibernationRow, HibernationStore};
pub use host::HostStore;
pub use intent::{IntentRow, IntentStore};
pub use notification::NotificationStore;
pub use placement::{
    HealthMarker, HostCapacityRow, PlacementEventRow, PlacementStore, ReserveOutcome, TenancyMode,
    TenancyRow, TenancyState,
};
pub use pool::PoolStore;
pub use proxy::ProxyStore;
pub use trust::{RepoTrustRow, RepoTrustStore};
pub use workspace::WorkspaceStore;
pub use zone::{ZoneDeleteOutcome, ZoneRow, ZoneStore};
