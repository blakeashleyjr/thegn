//! Weather refreshing, off the event loop.
//!
//! Threading contract: the whole pass — the cache read *and* the network round
//! trip — rides one blocking task, which builds its own current-thread runtime
//! *inside* itself (the [`crate::hydrate_calendar`] pattern) and pulses the
//! [`TerminalWaker`] once per delivery.
//!
//! Three rules differ from the other refreshers and are each easy to get wrong:
//!
//! 1. **Not on [`crate::sched::spawn_bg`].** That lane *silently skips* work
//!    when its eight permits are exhausted, on the assumption that a periodic
//!    trigger will retry shortly. The lane is busiest during startup — exactly
//!    when the one-shot first poll fires — and the retry here is *thirty
//!    minutes* away, so one dropped poll is half an hour of empty widget. This
//!    uses [`tokio::task::spawn_blocking`] directly, the same call and the same
//!    reasoning as `actions::spawn_usage`.
//! 2. **Cache first, always.** The cached snapshot is delivered before any
//!    network work is even considered, so a cold launch paints weather with no
//!    request at all — and a restart inside the refresh interval makes none.
//! 3. **A failure never touches the cache and never reaches the UI.** Last-good
//!    survives; the only trace is a `tracing::warn!` and `thegn doctor`. No
//!    status message and no toast: a keyless community service having a wobble
//!    is not something to interrupt the user about.

use termwiz::terminal::TerminalWaker;
use thegn_core::config_weather::WeatherConfig;
use thegn_core::connectivity::Connectivity;
use thegn_core::db::Db;
use thegn_core::seam::SeamError;
use thegn_core::store::WorkspaceStore;
use thegn_core::weather::WeatherSnapshot;
use tokio::sync::mpsc as tokio_mpsc;

use crate::hydrate::RefreshKind;

/// The `ui_state` scope every cached reading lives under, so the string has one
/// home and the reader and the writer cannot drift. The key beside it is
/// [`thegn_core::weather::cache_key`] — one row per (provider, location, units).
pub(crate) const CACHE_SCOPE: &str = "weather";

/// Consider a weather refresh: deliver the cached snapshot, then fetch if it is
/// older than the (floored) refresh interval.
///
/// `locale` is the environment's locale string, used only to resolve
/// `units = "auto"`; it is read on the loop (a cheap `env::var`) and passed in
/// so nothing here reaches for process state.
pub(crate) fn spawn_poll(
    cfg: WeatherConfig,
    locale: Option<String>,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || poll(cfg, locale, &tx, &waker));
}

/// The pass itself, split from the spawn so the blocking body is readable.
fn poll(
    cfg: WeatherConfig,
    locale: Option<String>,
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
) {
    // Belt-and-braces: the ticker already emits no slot at all when weather is
    // inert, so this only catches a programmatically-built config.
    if !cfg.is_active() {
        return;
    }
    let units = cfg.resolved_units(locale.as_deref());
    let key = thegn_core::weather::cache_key(cfg.provider.as_str(), &cfg.location, units);

    let db = match Db::open() {
        Ok(db) => Some(db),
        Err(e) => {
            // The cache is an accelerator, not the source of truth: without it
            // the fetch still runs, it just can't be suppressed or remembered.
            tracing::debug!(
                target: "thegn::weather",
                error = %e,
                "weather cache unavailable; polling without it"
            );
            None
        }
    };

    // Rule 2: whatever is cached goes to the UI before any network work is
    // considered, so a cold launch paints from disk.
    let cached = db.as_ref().and_then(|db| read_cache(db, &key));
    if let Some(snap) = &cached {
        deliver(tx, waker, snap.clone());
    }

    let offline = thegn_core::connectivity::current() == Connectivity::Offline;
    if !should_fetch(
        &cfg,
        cached.as_ref().map(|s| s.fetched_at),
        thegn_core::util::now(),
        offline,
    ) {
        return;
    }

    let Some(provider) = thegn_svc::weather::provider_for(&cfg, units) else {
        return;
    };
    let provider_id = provider.provider_id();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // Practically unreachable (an fd/thread exhaustion), but silence
            // here would be indistinguishable from "the service was quiet" —
            // and the cached reading has already been delivered, so the only
            // cost of saying so is one log line.
            tracing::debug!(
                target: "thegn::weather",
                error = %e,
                "no runtime for the weather fetch; keeping the cached reading"
            );
            return;
        }
    };
    match rt.block_on(provider.fetch()) {
        Ok(snap) => {
            thegn_core::connectivity::report_success();
            if let Some(db) = db.as_ref() {
                write_cache(db, &key, &snap);
            }
            deliver(tx, waker, snap);
        }
        Err(e) => {
            // Only a transport failure is evidence about the link: an `Api` or a
            // `Parse` means the service answered, so flipping the whole app to
            // "offline" over one would be a lie.
            if e.is_transient() {
                thegn_core::connectivity::report_failure();
            }
            // Rule 3: warn and return, sending nothing. The provider id and the
            // error are safe to print — neither ever carries the configured
            // location, which is the one piece of user data this feature holds.
            tracing::warn!(
                target: "thegn::weather",
                provider = provider_id,
                error = %e,
                "weather fetch failed — keeping the cached reading"
            );
        }
    }
}

/// Whether the network round trip is worth making, given what the cache holds.
///
/// Pure, and split out from [`poll`] so the whole decision is testable without a
/// runtime, a DB or a socket. `cached_at` is the cached snapshot's `fetched_at`
/// (`None` when nothing is cached); `now` is a parameter for the same reason it
/// is everywhere else in this feature.
///
/// Note what a `false` does **not** mean: the cached reading has already been
/// delivered by the time this is asked, so suppressing the fetch degrades to
/// last-good data, never to no data.
pub(crate) fn should_fetch(
    cfg: &WeatherConfig,
    cached_at: Option<i64>,
    now: i64,
    offline: bool,
) -> bool {
    if !cfg.is_active() {
        return false;
    }
    // A restart inside the interval costs zero requests. `refresh_secs` is
    // already floored at `MIN_REFRESH_SECS`, so a stray `0` cannot turn this
    // into "always fetch".
    if let Some(at) = cached_at
        && now - at < cfg.refresh_secs() as i64
    {
        return false;
    }
    // Offline: nothing to gain from the attempt, and its failure would decay the
    // recovery backoff for no reason.
    !offline
}

/// Read the cached snapshot for one configuration.
///
/// A row that fails to deserialize is from an older or newer shape; drop it and
/// let the fetch replace it, rather than failing the pass.
fn read_cache(db: &Db, key: &str) -> Option<WeatherSnapshot> {
    let json = match db.get_ui_state(CACHE_SCOPE, key) {
        Ok(v) => v?,
        Err(e) => {
            tracing::debug!(target: "thegn::weather", error = %e, "weather cache read failed");
            return None;
        }
    };
    serde_json::from_str(&json).ok()
}

/// Persist a fresh reading. Best-effort in both directions: the provider is the
/// source of truth and the DB is only a cache, so a write failure costs the next
/// launch one request and nothing else.
fn write_cache(db: &Db, key: &str, snap: &WeatherSnapshot) {
    let json = match serde_json::to_string(snap) {
        Ok(j) => j,
        Err(e) => {
            tracing::debug!(target: "thegn::weather", error = %e, "weather snapshot did not serialize");
            return;
        }
    };
    if let Err(e) = db.set_ui_state(CACHE_SCOPE, key, &json) {
        tracing::debug!(target: "thegn::weather", error = %e, "weather cache write failed");
    }
}

/// Hand a reading to the loop and wake it.
fn deliver(
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    snap: WeatherSnapshot,
) {
    if tx.send(RefreshKind::Weather(Box::new(snap))).is_err() {
        // The loop is gone — there is nothing left to wake, and nothing to
        // report the failure to.
        return;
    }
    if let Err(e) = waker.wake() {
        // best-effort: the loop may already be shutting down, so a missed pulse
        // costs at most a deferred paint.
        tracing::debug!(target: "thegn::weather", error = %e, "waker pulse failed");
    }
}

#[cfg(test)]
#[path = "hydrate_weather_tests.rs"]
mod tests;
