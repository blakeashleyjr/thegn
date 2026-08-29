//! Per-session agent error state, shared between the daemon's `SessionActor`
//! (which classifies each output chunk) and the host's `collect_attention` (which
//! projects the state into the worktree's `AttentionInputs`). The cache is
//! **host-side**: the compositor's `collect_attention` reads it directly, and a
//! subscription bridge over the daemon's event feed keeps it current.
//!
//! Two writers, one reader:
//!
//! - **The session actor** writes directly in the in-process case (the
//!   actor and the host share a process — the unit-test `Harness` path).
//!   The test path uses the same writer the production path uses for the
//!   daemon-disabled-but-tests-with-an-actor case: tests that build a
//!   real `SessionActor` and exercise the byte funnel.
//! - **A subscription bridge** (the `bridge::subscribe_loop` task) decodes
//!   `SessionActivityEvent` frames from the daemon's event feed and updates
//!   the cache. This is the cross-process path (default `[daemon] enabled =
//!   true` — daemon and host are separate processes).
//!
//! `collect_attention` only reads; it walks the cache once per hydration
//! pass and groups by worktree, so a worktree is `agent_error_active` iff at
//! least one of its sessions has `error_active = true`.
//!
//! The cache is process-global (`OnceLock<Mutex<…>>`) so neither the
//! subscription bridge nor `collect_attention` have to thread the handle
//! through every call site. The mutex is brief — one insert or one short
//! read per call — so contention is essentially zero.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use termwiz::terminal::TerminalWaker;

use crate::hydrate::RefreshKind;

/// One session's worth of agent-error state. The actor owns the canonical
/// value; the host-side cache mirrors it (same-process write) or is filled
/// by the subscription bridge (cross-process). `None` for `worktree` means
/// the session is unattributed (e.g. an unattached ephemeral) and never
/// raises the glyph on its own — the cache still holds it so the
/// subscription bridge's `SessionExit` handler can drop the entry on
/// teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentErrorEntry {
    /// Unique bridge/daemon generation that owns this entry. A reconnect gets
    /// a new owner so a late disconnect cannot clear newer state.
    pub owner: String,
    /// Worktree this session belongs to (`meta.worktree`), if any.
    pub worktree: Option<String>,
    /// Whether the session has emitted a harness failure banner that has
    /// not yet been cleared by resumed normal output.
    pub error_active: bool,
}

type Cache = Mutex<HashMap<String, AgentErrorEntry>>;

struct RefreshTarget {
    tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: Option<TerminalWaker>,
}

fn cell() -> &'static Cache {
    static CELL: OnceLock<Cache> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn refresh_target() -> &'static Mutex<Option<RefreshTarget>> {
    static TARGET: OnceLock<Mutex<Option<RefreshTarget>>> = OnceLock::new();
    TARGET.get_or_init(|| Mutex::new(None))
}

/// Install the compositor's existing model-refresh sink. The daemon process
/// never calls this; its cache writes therefore remain local and cheap.
pub(crate) fn install_refresh(
    tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    if let Ok(mut target) = refresh_target().lock() {
        *target = Some(RefreshTarget {
            tx,
            waker: Some(waker),
        });
    }
}

fn pulse_refresh() {
    if let Ok(target) = refresh_target().lock()
        && let Some(target) = target.as_ref()
    {
        let _ = target.tx.send(RefreshKind::Model); // best-effort: the compositor may be shutting down
        if let Some(waker) = target.waker.as_ref() {
            let _ = waker.wake(); // best-effort: a cache transition must not fail the producer
        }
    }
}

/// Update (or insert) one session's error state. Called by the actor in
/// the in-process path and by the subscription bridge after decoding an
/// `Activity` frame. `worktree == None` is allowed (an unattributed
/// session) but won't light a sidebar row.
pub fn set(session: &str, worktree: Option<String>, error_active: bool) {
    set_for("local", session, worktree, error_active);
}

/// Update one session for a particular bridge/daemon generation.
pub fn set_for(owner: &str, session: &str, worktree: Option<String>, error_active: bool) {
    let changed = if let Ok(mut g) = cell().lock() {
        // A stream from an older daemon generation can still have buffered
        // Activity frames when a replacement connection has installed its
        // snapshot. Never let that late frame reclaim the newer entry.
        if g.get(session)
            .is_some_and(|existing| existing.owner != owner)
        {
            false
        } else {
            // Skip the lock-and-store when the value is unchanged: a chatty
            // agent that keeps emitting the same error banner can otherwise
            // produce a cache write per chunk. Same-key + same-value is a
            // no-op for the reader.
            if let Some(existing) = g.get(session)
                && existing.worktree == worktree
                && existing.error_active == error_active
            {
                false
            } else {
                g.insert(
                    session.to_string(),
                    AgentErrorEntry {
                        owner: owner.to_string(),
                        worktree,
                        error_active,
                    },
                );
                true
            }
        }
    } else {
        false
    };
    if changed {
        pulse_refresh();
    }
}

/// Replace all entries owned by `owner` from an authoritative session-list
/// snapshot. Returns whether the visible cache changed and emits one refresh
/// for the whole snapshot rather than one per session.
pub fn replace_owner(
    owner: &str,
    sessions: impl IntoIterator<Item = (String, Option<String>, bool)>,
) {
    let next: HashMap<String, AgentErrorEntry> = sessions
        .into_iter()
        .map(|(session, worktree, error_active)| {
            (
                session,
                AgentErrorEntry {
                    owner: owner.to_string(),
                    worktree,
                    error_active,
                },
            )
        })
        .collect();
    let changed = if let Ok(mut g) = cell().lock() {
        let old: HashMap<_, _> = g
            .iter()
            .filter(|(_, entry)| entry.owner == owner)
            .map(|(session, entry)| (session.clone(), entry.clone()))
            .collect();
        let changed = old != next;
        g.retain(|_, entry| entry.owner != owner);
        g.extend(next);
        changed
    } else {
        false
    };
    if changed {
        pulse_refresh();
    }
}

/// Drop one session only when it still belongs to this bridge generation.
pub fn clear_for(owner: &str, session: &str) {
    let changed = if let Ok(mut g) = cell().lock() {
        if g.get(session).is_some_and(|entry| entry.owner == owner) {
            g.remove(session);
            true
        } else {
            false
        }
    } else {
        false
    };
    if changed {
        pulse_refresh();
    }
}

/// Drop all entries owned by one bridge/daemon generation after its stream
/// ends. A newer connection has a different owner and is left untouched.
pub fn clear_owner(owner: &str) {
    let changed = if let Ok(mut g) = cell().lock() {
        let before = g.len();
        g.retain(|_, entry| entry.owner != owner);
        before != g.len()
    } else {
        false
    };
    if changed {
        pulse_refresh();
    }
}

/// Legacy/local teardown helper. Bridge teardown uses [`clear_for`] so a late
/// event cannot remove a newer daemon's entry for the same session id.
pub fn clear(session: &str) {
    clear_for("local", session);
}

/// True iff any session whose `worktree` equals `wt_path` has
/// `error_active = true`. The single read `collect_attention` does per
/// worktree; a worktree without any session in the cache never sees the
/// glyph (which is the pre-THE-89 behaviour).
pub fn worktree_has_error(wt_path: &str) -> bool {
    cell()
        .lock()
        .map(|g| {
            g.values()
                .any(|e| e.worktree.as_deref() == Some(wt_path) && e.error_active)
        })
        .unwrap_or(false)
}

/// Drop every entry in the cache. Test-only — production code never needs
/// to wipe a process-global. Kept behind a `#[cfg(test)]` boundary so a
/// stray production caller fails to compile.
#[cfg(test)]
pub fn clear_all() {
    if let Ok(mut g) = cell().lock() {
        g.clear();
    }
}

#[cfg(test)]
fn install_refresh_test_sink(tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>) {
    if let Ok(mut target) = refresh_target().lock() {
        *target = Some(RefreshTarget { tx, waker: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    /// The cache deduplicates same-key + same-value writes (a chatty agent
    /// keeps emitting the same error banner) so the reader never sees a
    /// write amplification per chunk.
    #[test]
    fn same_value_writes_are_a_noop() {
        let _serial = test_lock();
        clear_all();
        set("s1", Some("/wt/a".into()), true);
        // Same value → no change; a follow-up read sees the same state.
        set("s1", Some("/wt/a".into()), true);
        assert!(worktree_has_error("/wt/a"));
        clear("s1");
    }

    /// A worktree with no entries in the cache is a no-signal worktree —
    /// exactly the pre-THE-89 behaviour for the in-process case (the
    /// daemon's writes live in its OWN process, so the compositor's cache
    /// is empty when the bridge hasn't been wired up).
    #[test]
    fn an_empty_cache_reports_no_errors() {
        let _serial = test_lock();
        clear_all();
        assert!(!worktree_has_error("/wt/nobody"));
    }

    /// `clear` on an absent session is a no-op; absence is the natural
    /// post-condition.
    #[test]
    fn clearing_an_absent_session_is_a_noop() {
        let _serial = test_lock();
        clear_all();
        clear("never-was");
        assert!(!worktree_has_error("/wt/anything"));
    }

    /// A worktree with one active session + one clear session is still
    /// `error_active` (the other one is still raising).
    #[test]
    fn one_active_session_lights_the_worktree() {
        let _serial = test_lock();
        clear_all();
        set("s1", Some("/wt/a".into()), true);
        set("s2", Some("/wt/a".into()), false);
        assert!(worktree_has_error("/wt/a"));
        // A different worktree, even with active sessions, is unaffected.
        set("s3", Some("/wt/b".into()), true);
        assert!(worktree_has_error("/wt/b"));
        clear_all();
    }

    /// An unattributed session (`worktree: None`) never raises a worktree
    /// glyph — it lives in the cache so the bridge can clean it up on
    /// teardown, but it doesn't light any sidebar row on its own.
    #[test]
    fn an_unattributed_session_does_not_light_a_worktree() {
        let _serial = test_lock();
        clear_all();
        set("s1", None, true);
        assert!(!worktree_has_error("/wt/anything"));
        clear_all();
    }

    #[test]
    fn disconnect_clears_only_the_bridge_generation_that_disconnected() {
        let _serial = test_lock();
        clear_all();
        set_for("old", "s1", Some("/wt/a".into()), true);
        replace_owner("new", [("s1".into(), Some("/wt/a".into()), true)]);
        clear_owner("old");
        assert!(worktree_has_error("/wt/a"));
        clear_owner("new");
        assert!(!worktree_has_error("/wt/a"));
    }

    #[test]
    fn late_old_generation_cannot_overwrite_newer_entry() {
        let _serial = test_lock();
        clear_all();
        set_for("new", "s1", Some("/wt/a".into()), false);
        set_for("old", "s1", Some("/wt/a".into()), true);
        assert!(!worktree_has_error("/wt/a"));
        clear_all();
    }

    #[test]
    fn reconnect_snapshot_replaces_stale_entries() {
        let _serial = test_lock();
        clear_all();
        set_for("fresh", "stale", Some("/wt/a".into()), true);
        replace_owner("fresh", [("current".into(), Some("/wt/b".into()), true)]);
        assert!(!worktree_has_error("/wt/a"));
        assert!(worktree_has_error("/wt/b"));
        clear_all();
    }

    #[tokio::test]
    async fn changed_bits_schedule_one_model_refresh_each() {
        let _serial = test_lock();
        clear_all();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        install_refresh_test_sink(tx);

        set_for("bridge", "s1", Some("/wt/a".into()), true);
        assert!(matches!(rx.try_recv(), Ok(RefreshKind::Model)));
        // Repeating the same bit is coalesced.
        set_for("bridge", "s1", Some("/wt/a".into()), true);
        assert!(rx.try_recv().is_err());
        set_for("bridge", "s1", Some("/wt/a".into()), false);
        assert!(matches!(rx.try_recv(), Ok(RefreshKind::Model)));
        clear_owner("bridge");
        assert!(matches!(rx.try_recv(), Ok(RefreshKind::Model)));
        clear_all();
    }
}
