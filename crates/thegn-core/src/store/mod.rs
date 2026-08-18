//! Backend-agnostic repository traits — the **store seam**.
//!
//! thegn's local state lives in an embedded SQLite database
//! ([`crate::db::Db`], implemented over rusqlite). These traits factor that DB
//! API surface into cohesive, domain-scoped seams so that:
//!
//!   * a future **server-side / multi-user** thegn can supply a
//!     Postgres-backed implementation (e.g. via diesel) *without* the
//!     single-user shell taking on any Postgres/async weight, and
//!   * a future embedded-engine swap (e.g. turso, once it ships a production
//!     release) is a localized new `impl`, not a scattered rewrite.
//!
//! The seam is **sync** on purpose — `thegn-core` deliberately carries no
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
//! [`PoolStore`] (warm-spare pool), and [`HostStore`] (host state machine).

mod account;
mod cache;
mod compute;
mod control;
mod hibernate;
mod host;
mod intent;
mod notification;
mod placement;
mod pool;
mod semantic;
mod trust;
mod workspace;
// NOT `aux` — `AUX` is a reserved DOS device name, so Windows git refuses to
// create the file at all ("invalid path"), which made the repo unclonable on
// Windows and failed the msvc CI job at checkout. Same trap: con, prn, nul,
// com1-9, lpt1-9 (the extension is irrelevant).
mod worktree_aux;
mod zone;

pub use account::AccountStore;
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
pub use semantic::{SemEdgeRow, SemEntityRow, SemanticStore};
pub use trust::{RepoTrustRow, RepoTrustStore};
pub use workspace::WorkspaceStore;
pub use worktree_aux::WorktreeAuxStore;
pub use zone::{ZoneDeleteOutcome, ZoneRow, ZoneStore};
