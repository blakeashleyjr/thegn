//! The **notification** seam: the notification feed (unread/alert rollups
//! per worktree) and the agent-dispatch registry (which worktree an agent task
//! was dispatched to, and its status).

use anyhow::Result;

/// Object-safe (`&self` + concrete args), so `&dyn NotificationStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait NotificationStore {
    /// Append a notification.  Returns the new row id.
    fn put_notification(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<i64>;

    /// Append a notification only if an identical `(kind, issue_id, message)`
    /// row doesn't already exist (emit-once for re-derived facts like overdue
    /// / mentions). Returns whether a row was inserted.
    fn put_notification_once(
        &self,
        kind: &str,
        issue_id: &str,
        message: &str,
        worktree_path: &str,
    ) -> Result<bool>;

    /// Whether a notification with this `(kind, issue_id)` already exists.
    ///
    /// `put_notification` is a plain insert, so a producer that can be re-run
    /// (a restart, a re-sync) needs this to stay idempotent. Calendar reminders
    /// encode `(event, occurrence, lead time)` into `issue_id` for exactly this
    /// — no extra table required.
    fn has_notification(&self, kind: &str, issue_id: &str) -> Result<bool>;

    /// All unread notifications, newest first.
    fn get_unread_notifications(&self) -> Result<Vec<crate::notification::Notification>>;

    /// All notifications (read and unread), newest first, capped at `limit`.
    fn get_all_notifications(&self, limit: usize)
    -> Result<Vec<crate::notification::Notification>>;

    /// Mark a single notification as read.
    fn mark_notification_read(&self, id: i64) -> Result<()>;

    /// Mark all notifications as read.
    fn mark_all_notifications_read(&self) -> Result<()>;
    /// Mark read exactly what the repo-scoped inbox DISPLAYS — the same three
    /// arms as [`crate::notification_scope::shows_in_repo_inbox`]: untagged
    /// (host-global) rows, rows tagged with one of `repo_paths`, and rows tagged
    /// with a path `all_known` does not contain (fail-open: the main checkout,
    /// an externally-created worktree). Passing `all_known` is what makes the
    /// clear and the display agree; before THE-68 the clear omitted the
    /// fail-open arm, so those rows were shown forever and `a` never cleared
    /// them. The unscoped [`Self::mark_all_notifications_read`] stays for the
    /// all-worktrees (`g`) view — a repo-scoped inbox's "clear all" must not
    /// silently mark OTHER repos' notifications read.
    fn mark_notifications_read_scoped(
        &self,
        repo_paths: &[String],
        all_known: &[String],
    ) -> Result<()>;
    /// Mark every notification for one worktree read (the unified surface's
    /// per-row `x`: quieting a worktree's needs-you signal must also retire its
    /// inbox rows, or the same item reappears under Alerts).
    fn mark_notifications_read_for_worktree(&self, worktree_path: &str) -> Result<()>;

    /// Get unread notification counts grouped by worktree_path.
    /// Returns a map from worktree_path to count of unread notifications.
    /// Unread notification counts grouped by worktree, restricted to `counted_kinds`
    /// (the config-derived non-`info` kinds). Informational kinds are excluded by
    /// passing only the counted set, so lifecycle events never inflate the badge.
    /// An empty slice yields an empty map.
    fn get_unread_counts_by_worktree(
        &self,
        counted_kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>>;

    /// Alert counts grouped by worktree, restricted to `alert_kinds` (the
    /// config-derived `alert`-priority kinds). Drives the red ⚑ flag badge. An
    /// empty slice yields an empty map (no flag).
    fn get_alert_counts_by_worktree(
        &self,
        alert_kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, usize>>;

    /// Delete a single notification row (dismiss).
    fn delete_notification(&self, id: i64) -> Result<()>;

    /// Acknowledge (quiet) a worktree's live "Needs you" signal: UPSERT the
    /// `(reason, since, episode)` currently showing so it's suppressed until that
    /// episode changes. `reason` is the serde-encoded
    /// [`crate::attention::AttentionReason`]; keyed per `(worktree, reason)` so
    /// acking one signal never destroys the ack for another on the same worktree.
    fn put_attention_ack(
        &self,
        worktree_path: &str,
        reason: &str,
        since: Option<i64>,
        episode: crate::attention::Episode,
    ) -> Result<()>;

    /// Every stored attention ack. The host matches these against fresh scores;
    /// a non-match means only "that signal isn't the winner right now", so the
    /// read pass never deletes — see [`crate::attention::ack_expired`].
    fn list_attention_acks(&self) -> Result<Vec<crate::attention::AttentionAckRow>>;

    /// Drop attention acks for a worktree — one `reason`, or all of them when
    /// `reason` is `None` (worktree removed / explicit un-ack).
    fn delete_attention_ack(&self, worktree_path: &str, reason: Option<&str>) -> Result<()>;

    /// Raise (or refresh) a session's hand. Upsert on the `session` primary
    /// key: a re-raise replaces, so the table holds at most one row per
    /// session — the append-only inbox is what THE-68 replaced.
    fn put_session_attention(&self, a: &crate::osc_attention::SessionAttention) -> Result<()>;

    /// Lower one session's hand (the user answered, or the session ended).
    fn clear_session_attention(&self, session: &str) -> Result<()>;

    /// Lower every hand raised for one worktree. The per-worktree ack and
    /// "clear all" call this: quieting a worktree must retire the live signal
    /// too, or the new state becomes a new un-clearable nag.
    /// Returns the rows removed.
    fn clear_session_attention_for_worktree(&self, worktree_path: &str) -> Result<usize>;

    /// Empty the table. Called where the session registry is created empty
    /// (daemon boot; host boot with `[daemon] enabled = false`) — no live
    /// sessions means no live hands.
    fn clear_all_session_attention(&self) -> Result<()>;

    /// Every hand currently up. One small table read on the hydration worker,
    /// beside `list_merge_queue` / `list_dispatches`.
    fn list_session_attention(&self) -> Result<Vec<crate::osc_attention::SessionAttention>>;

    /// Drop rows older than `max_age_secs` — a table-growth bound only; a hand
    /// is lowered by an answer or a session ending, never by this sweep.
    /// Returns the rows removed.
    fn prune_session_attention(&self, max_age_secs: i64) -> Result<usize>;

    /// Drop acks older than `max_age_secs`. A table-growth bound only: acks are
    /// released by a new episode, not by this sweep. Returns the rows removed.
    fn prune_attention_acks(&self, max_age_secs: i64) -> Result<usize>;

    /// Record a new agent dispatch.  Returns the new row id.
    ///
    /// Takes the whole row as [`NewDispatch`](crate::issue::NewDispatch) rather
    /// than positional arguments — the insert carries seven fields, four of them
    /// optional, which is exactly the shape that mis-orders same-typed strings
    /// at a call site.
    fn put_agent_dispatch(&self, new: crate::issue::NewDispatch<'_>) -> Result<i64>;

    /// Update the status of a dispatch. Takes the **typed** status (never a
    /// free string) so the roster's status column stays a closed, parseable set
    /// — the persistence layer stores its `as_str()` form.
    fn update_dispatch_status(
        &self,
        id: i64,
        status: crate::issue::AgentDispatchStatus,
    ) -> Result<()>;

    /// Stamp a dispatched row with the session running it and the artifact it
    /// will produce. The roster's only field update: `session_id` is the row's
    /// identity for pane-exit attribution (`dispatch_for_exit`) and
    /// `artifact_path` is the pointer the completion gate checks, and neither
    /// is knowable until the row id exists and the session has opened.
    fn stamp_dispatch_run(&self, id: i64, session_id: &str, artifact_path: &str) -> Result<()>;

    /// Write (replace, not append — the caller composes any append) the
    /// `note` free-text on a dispatch row. The transport-retry observer's
    /// ledger (THE-86): why a headless worker died, which retry attempt it
    /// reached, or why a relaunch failed. The ONLY writer is the daemon
    /// stamper, and it never touches the status toward a terminal state.
    fn stamp_dispatch_note(&self, id: i64, note: &str) -> Result<()>;

    /// The dispatch row run by a daemon session id (most recent stamp wins),
    /// or `None`. The transport-retry observer's row resolution (THE-86):
    /// `session_id` is stamped at launch and re-stamped on every relaunch, so
    /// it is the row's current identity while it is in flight. Callers filter
    /// terminal rows themselves (a row the pane path or the Lead already
    /// closed must never be re-touched).
    fn dispatch_by_session(&self, session_id: &str) -> Result<Option<crate::issue::AgentDispatch>>;

    /// The whole agent-dispatch roster, newest first — the durable orchestration
    /// ledger a restarted supervisor reads back to resume without
    /// re-dispatching. Stored status strings are coerced through
    /// [`crate::issue::AgentDispatchStatus::parse`], so a legacy or corrupt row
    /// lists as `Unknown` rather than failing the read.
    fn list_dispatches(&self) -> Result<Vec<crate::issue::AgentDispatch>>;

    /// One dispatch row by id (typed), or `None` if it does not exist.
    fn get_dispatch(&self, id: i64) -> Result<Option<crate::issue::AgentDispatch>>;

    /// Find the dispatch id for a worktree path (most recent, if any).
    fn dispatch_for_worktree(&self, worktree_path: &str) -> Result<Option<i64>>;

    /// The dispatch timestamp (`dispatched_at_ms`) of a worktree's most recent
    /// agent dispatch, if any. Read at resurrection to age a persisted
    /// running/active agent signal through [`crate::activity::coerce_stale`], so a
    /// phantom forever-running dot from a session killed mid-run is downgraded.
    fn dispatch_dispatched_at_ms(&self, worktree_path: &str) -> Result<Option<i64>>;

    /// Find the dispatch id and originating issue id for a worktree path.
    fn dispatch_info_for_worktree(&self, worktree_path: &str) -> Result<Option<(i64, String)>>;

    /// Resolve the dispatch row a finished worker belonged to — `(id, issue_id)`
    /// — for the pane-exit handler that stamps `Done`/`Failed`.
    ///
    /// Two rules, in order:
    ///
    /// 1. **`session_id` exact match.** A dispatch launched through
    ///    `sessions.open` records the daemon session running it, and that is the
    ///    row's identity. Once a pipeline runs several stages in ONE worktree,
    ///    the path alone cannot say which row just died.
    /// 2. **Most recent *active* row for the worktree**, for a worker with no
    ///    recorded session (the `D` key, a hand-run agent pane). Terminal rows
    ///    (`done`/`failed`/`merged`/`abandoned`) are SKIPPED: re-stamping a
    ///    finished row is how a plain shell opened later in an ex-agent worktree
    ///    used to overwrite the outcome — and re-fire an "agent finished"
    ///    notification — for work that ended days ago.
    ///
    /// `None` when neither rule matches, which the caller must treat as "not an
    /// agent pane" rather than as an error.
    fn dispatch_for_exit(
        &self,
        worktree_path: &str,
        session_id: Option<&str>,
    ) -> Result<Option<(i64, String)>>;
}
