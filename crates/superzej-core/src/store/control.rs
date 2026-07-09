//! Control-plane registry seam: running pane daemons, session leases, and
//! pairing credentials (v40; see [`crate::db_control`] for the SQLite impl).
//!
//! git stays the source of truth for worktrees; these tables are pure
//! cache/coordination state — a daemon registers itself so clients can discover
//! and attach, leases record the grace period that keeps a detached session's
//! PTY warm, and pairings hold the **hashed** scoped tokens thin clients redeem.

use anyhow::Result;

/// A running pane daemon, registered so clients can discover and attach.
/// `endpoint` is the unix control socket; `tcp_addr` is set while `szhost serve`
/// is listening for remote thin clients. All times are unix **ms**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonRow {
    pub daemon_id: String,
    pub pid: i64,
    /// Scope key: the canonicalized state dir this daemon serves (one daemon
    /// per `$XDG_STATE_HOME/superzej`).
    pub scope: String,
    /// Unix control-socket path.
    pub endpoint: String,
    /// `host:port` while serve mode listens for remote clients, else `None`.
    pub tcp_addr: Option<String>,
    pub hostname: String,
    pub version: String,
    pub started_at: i64,
    pub heartbeat_at: i64,
}

/// A session lease: `kind = "attached"` while a client holds the session
/// (`expires_at` is `NULL` — it lives until detach), `kind = "relay"` for the
/// grace period keeping a detached session's PTY warm (`expires_at` set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub lease_id: i64,
    pub session_id: String,
    pub daemon_id: String,
    /// The holding client for `attached` leases; `None` for `relay`.
    pub client_id: Option<String>,
    pub kind: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

/// A pairing credential. `kind = "code"` is the single-use secret embedded in a
/// pairing URL; redeeming it mints a `kind = "token"` row (the long-lived scoped
/// bearer credential, `parent_id` pointing back at the code). Only the sha-256
/// hex of the secret half is stored — never plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRow {
    /// The token's public id half (lookup key; safe to log).
    pub pairing_id: String,
    pub kind: String,
    pub token_hash: String,
    /// [`crate::control::ScopeSet`] in csv form, e.g. `"read,git"`.
    pub scope: String,
    pub label: String,
    pub parent_id: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub redeemed_at: Option<i64>,
    /// A `token` row authorizes requests only once approved. Auto-approve
    /// pairing sets this at mint time; `[serve] require_approval` parks it
    /// `NULL` until an in-app / `szhost pair approve` decision.
    pub approved_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

/// Daemon registry + session leases + pairings — the control-plane store.
///
/// Sync like every store seam (`superzej-core` carries no tokio); the daemon
/// and clients call it off their hot paths. `now_ms` is always injected so the
/// time-dependent queries stay deterministic under test.
pub trait ControlStore {
    // --- daemon registry ---
    /// Register (or replace, keyed by `daemon_id`) a running daemon.
    fn put_daemon(&self, row: &DaemonRow) -> Result<()>;
    fn daemons(&self) -> Result<Vec<DaemonRow>>;
    fn del_daemon(&self, daemon_id: &str) -> Result<()>;
    fn touch_daemon_heartbeat(&self, daemon_id: &str, now_ms: i64) -> Result<()>;
    /// Daemons for `scope` whose heartbeat is within `ttl_ms` of `now_ms` — the
    /// discovery query clients attach through.
    fn live_daemons(&self, scope: &str, now_ms: i64, ttl_ms: i64) -> Result<Vec<DaemonRow>>;

    // --- session leases ---
    /// Open a lease; returns its rowid.
    fn put_lease(
        &self,
        session_id: &str,
        daemon_id: &str,
        client_id: Option<&str>,
        kind: &str,
        expires_at: Option<i64>,
        now_ms: i64,
    ) -> Result<i64>;
    fn leases(&self, daemon_id: &str) -> Result<Vec<LeaseRow>>;
    fn refresh_lease(&self, lease_id: i64, expires_at: i64) -> Result<()>;
    fn release_lease(&self, lease_id: i64) -> Result<()>;
    /// Release every lease for a session (attach cancels its relay lease).
    fn release_session_leases(&self, session_id: &str) -> Result<()>;
    /// Delete-and-return the expired relay leases for `daemon_id` — the daemon
    /// reaps the returned sessions' PTYs.
    fn reap_expired_leases(&self, daemon_id: &str, now_ms: i64) -> Result<Vec<LeaseRow>>;
    /// Boot-time sweep: drop every lease owned by `daemon_id` (a restarted
    /// daemon's PTYs died with the old process, so its leases are meaningless).
    fn clear_daemon_leases(&self, daemon_id: &str) -> Result<()>;

    // --- pairings ---
    fn put_pairing(&self, row: &PairingRow) -> Result<()>;
    fn pairings(&self) -> Result<Vec<PairingRow>>;
    /// Live-token lookup by public id for request auth: `kind = "token"`,
    /// approved, unrevoked, unexpired at `now_ms`. The caller compares the
    /// secret hash.
    fn pairing_for_auth(&self, pairing_id: &str, now_ms: i64) -> Result<Option<PairingRow>>;
    /// Approve a parked `token` row (the `[serve] require_approval` flow).
    fn approve_pairing(&self, pairing_id: &str, now_ms: i64) -> Result<()>;
    /// Atomic single-use redeem of a `kind = "code"` row: returns it iff it was
    /// live (unexpired, unrevoked) and unredeemed, marking `redeemed_at` in the
    /// same UPDATE so a racing second redeem gets `None` (no TOCTOU).
    fn redeem_pairing_code(&self, pairing_id: &str, now_ms: i64) -> Result<Option<PairingRow>>;
    fn revoke_pairing(&self, pairing_id: &str, now_ms: i64) -> Result<()>;
}
