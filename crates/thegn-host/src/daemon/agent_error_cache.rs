//! Per-session agent error state, shared between the daemon's [`SessionActor`]
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

/// One session's worth of agent-error state. The actor owns the canonical
/// value; the host-side cache mirrors it (same-process write) or is filled
/// by the subscription bridge (cross-process). `None` for `worktree` means
/// the session is unattributed (e.g. an unattached ephemeral) and never
/// raises the glyph on its own — the cache still holds it so the
/// subscription bridge's `SessionExit` handler can drop the entry on
/// teardown.
#[derive(Debug, Clone)]
pub struct AgentErrorEntry {
    /// Worktree this session belongs to (`meta.worktree`), if any.
    pub worktree: Option<String>,
    /// Whether the session has emitted a harness failure banner that has
    /// not yet been cleared by resumed normal output.
    pub error_active: bool,
}

type Cache = Mutex<HashMap<String, AgentErrorEntry>>;

fn cell() -> &'static Cache {
    static CELL: OnceLock<Cache> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Update (or insert) one session's error state. Called by the actor in
/// the in-process path and by the subscription bridge after decoding an
/// `Activity` frame. `worktree == None` is allowed (an unattributed
/// session) but won't light a sidebar row.
pub fn set(session: &str, worktree: Option<String>, error_active: bool) {
    if let Ok(mut g) = cell().lock() {
        // Skip the lock-and-store when the value is unchanged: a chatty
        // agent that keeps emitting the same error banner can otherwise
        // produce a cache write per chunk. Same-key + same-value is a
        // no-op for the reader.
        if let Some(existing) = g.get(session)
            && existing.worktree == worktree
            && existing.error_active == error_active
        {
            return;
        }
        g.insert(
            session.to_string(),
            AgentErrorEntry {
                worktree,
                error_active,
            },
        );
    }
}

/// Drop one session's entry — the actor's teardown handler or the bridge's
/// `SessionExit` handler. Safe to call on an absent id; absent means absent.
pub fn clear(session: &str) {
    if let Ok(mut g) = cell().lock() {
        g.remove(session);
    }
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
mod tests {
    use super::*;

    /// The cache deduplicates same-key + same-value writes (a chatty agent
    /// keeps emitting the same error banner) so the reader never sees a
    /// write amplification per chunk.
    #[test]
    fn same_value_writes_are_a_noop() {
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
        clear_all();
        assert!(!worktree_has_error("/wt/nobody"));
    }

    /// `clear` on an absent session is a no-op; absence is the natural
    /// post-condition.
    #[test]
    fn clearing_an_absent_session_is_a_noop() {
        clear_all();
        clear("never-was");
        assert!(!worktree_has_error("/wt/anything"));
    }

    /// A worktree with one active session + one clear session is still
    /// `error_active` (the other one is still raising).
    #[test]
    fn one_active_session_lights_the_worktree() {
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
        clear_all();
        set("s1", None, true);
        assert!(!worktree_has_error("/wt/anything"));
        clear_all();
    }
}
