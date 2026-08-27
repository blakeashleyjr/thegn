# Chunk 2 — Core: the `session_attention` state table + the `agent_attention_inbox` knob

**Issue:** THE-68 (first half, core side). **Branch:** `tg/the-68-log-noise`.
**Depends on:** nothing. **Land order:** second (chunk 3 needs this API).
**Overlaps:** no files with chunks 1, 3 or 4.

Read `.thegn/pipeline/architect/design.md` §1, §3, §4 and §5 first. §5 fixes the
exact signatures chunk 3 is being written against — **do not rename them.**

This chunk builds the storage and the config knob and **wires nothing**. After it
lands, behaviour is byte-for-byte unchanged; chunk 3 flips the producer over.

---

## Why a state table instead of the notification row

`daemon/session.rs:754` persists every OSC raised hand as an `agent_attention`
notification, and uses that append-only log as the cross-process channel for
live state. That gives one row per agent turn (Claude Code emits `OSC 9` at the
end of every turn), a demand that never clears when the user answers, and a
write that bypasses `notify::record`'s routing/DND/debounce. A raised hand has
one value at a time — it belongs in a state table, deleted when the hand goes
down.

---

## Files

### 1. `crates/thegn-core/src/osc_attention.rs` — add the row type

Append to the existing module (it already owns `AttentionSignal`):

```rust
/// A raised hand that is still up: one row per daemon session, upserted when a
/// process emits `OSC 9`/`OSC 777;notify` and deleted the moment the user
/// answers. **Live state, not an inbox event** — the distinction that fixes
/// THE-68: an inbox row is an event you might miss, a raised hand is state you
/// can already see, and treating the second as the first filled the inbox with
/// one row per agent turn that no "clear all" could retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttention {
    /// The daemon session id — the row's identity, so a re-raise replaces.
    pub session: String,
    /// The worktree the hand is raised for. Never empty: a signal from a
    /// session with no worktree writes no row (it could light no sidebar row).
    pub worktree_path: String,
    /// The notification title, when the convention carried one ("" otherwise).
    pub title: String,
    /// The message body — what the agent is asking.
    pub body: String,
    /// When the hand went up, unix **seconds** (`crate::util::now`). The honest
    /// `since` the attention scorer sorts and renders "N ago" from.
    pub since: i64,
}
```

### 2. `crates/thegn-core/src/db.rs` — schema

Three edits, all small; do **not** grow this god-file beyond them.

**a. `SCHEMA_VERSION: i64 = 56` → `57`** (line 116).

> `SCHEMA_VERSION` bumps are a known collision point between branches. Rebase on
> `main` immediately before landing and re-check the number.

**b. DDL** in the `CREATE TABLE IF NOT EXISTS` batch, following the `kaneo_auth`
(v47) entry's comment style:

```sql
-- v57: live "raised hand" state, one row per daemon session. An OSC 9 /
-- OSC 777;notify signal is LIVE STATE — deleted the moment the user answers —
-- not an inbox event, so it no longer appends one `agent_attention`
-- notification per agent turn (THE-68). This row is the cross-process channel
-- from the session actor to the compositor's attention scorer. Pure cache:
-- reaped on answer, on session end, on daemon boot, on `del_worktree`, and by
-- the startup age sweep.
CREATE TABLE IF NOT EXISTS session_attention (
  session       TEXT PRIMARY KEY,
  worktree_path TEXT NOT NULL,
  title         TEXT NOT NULL,
  body          TEXT NOT NULL,
  since         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_session_attention_wt
  ON session_attention (worktree_path);
```

**c. One-time cleanup**, beside the `if ver < 46` block (`db.rs:846-857`), which
is the precedent to copy — comment style included:

```rust
// v57: one-time retirement of the `agent_attention` pile that accrued while
// every OSC raised hand appended an inbox row (one per agent turn, and the
// row never cleared when the user answered — see THE-68). Those rows are now
// live state in `session_attention`. Mark the unread ones read so the inbox
// and the ⚑/✋ counts start clean instead of carrying months of backlog.
// Gated on the pre-bump on-disk version so it runs exactly once; a deliberate
// `thegn notify push --urgency alert` raised after the upgrade is untouched.
// Best-effort: the DB is a cache, and a fresh DB matches zero rows.
if ver < 57 {
    let _ = conn.execute(
        "UPDATE notifications SET read=1 WHERE kind='agent_attention' AND read=0",
        [],
    );
}
```

**d. `startup_prune`** (`db.rs:887`): add the age sweep beside the ack sweep, and
extend that function's doc comment with a bullet for it.

```rust
// A raised hand outliving its session by a week is a leak, not a demand.
{
    use crate::store::NotificationStore as _;
    let _ = self.prune_session_attention(7 * 24 * 3600);
}
```

### 3. `crates/thegn-core/src/store/notification.rs` — the trait methods

Add the six from design §5, each with a doc comment. `Db` is the only impl
(`grep -rn "impl NotificationStore"` confirms), so this is contained.

```rust
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
```

### 4. `crates/thegn-core/src/db_notification.rs` — the impls

Plain `rusqlite`, matching the file's existing style. Upsert as
`INSERT INTO session_attention(...) VALUES(...) ON CONFLICT(session) DO UPDATE SET ...`.
`list_session_attention` orders by `since ASC` (longest-waiting first — it
matches the attention sort's tie-break and costs nothing here). `prune_*` uses
`since < ?1` against `crate::util::now() - max_age_secs`.

### 5. `crates/thegn-core/src/db_workspace.rs` — `del_worktree` cascade

Add a delete beside the `attention_acks` one (line ~345), with a matching
comment: a removed worktree must not leave a raised hand behind, or a worktree
recreated at the same path inherits an instant `Blocked` dot. Consider
`del_worktrees_for_repo` (line 373) too — mirror its `IN (SELECT …)` style.

### 6. `crates/thegn-core/src/config.rs` — the knob

`NotificationsConfig` (line 4458) gains one field; `Default` (line 4514) gains
`agent_attention_inbox: false`.

```rust
    /// Also record an OSC 9 / OSC 777 raised hand as an inbox row (an audit
    /// trail of every time an agent asked for you). **Off by default**: the
    /// raised hand is live state, already carried by the sidebar dot, the ✋
    /// chip and the "Needs you" ring, and agent CLIs emit one at the end of
    /// every turn — so the inbox filled with "Claude is waiting for your input"
    /// and buried everything else (THE-68). When on, the write is one CURRENT
    /// row per session (delete-then-insert), never one per turn.
    pub agent_attention_inbox: bool,
```

Do **not** add it to `NotificationsOverlay` (the `[profiles.*]` overlay) —
`github_mentions` is already absent from that struct, so there is no
completeness gate, and adding it means touching `is_empty` + `apply` + the
coverage test for no benefit.

**The env knob is mandatory, not optional.** `test/env-overlay-ratchet.txt` is
**SHRINK-ONLY** (see its header) and already pins all six `notifications.*` keys,
so a new shallow key cannot be pinned there — it must have a real override. In
`Config::env_overlay` (line 5633), following the `THEGN_DISK_SHOW_SIZES` pattern
at line 5771:

```rust
if let Some(v) = env.get("THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX") {
    o.notifications_agent_attention_inbox = parse_bool(&v, "THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX");
}
```

Add the matching `Option<bool>` field to the env `ConfigOverlay` struct and its
`apply`, following whichever flat-field naming that struct already uses for
`[disk]`/`[sandbox]` keys — read it, match it, don't invent.

### 7. `config/config.toml.example`

Document the key inside the existing `[notifications]` block (around line 1683,
next to the `surface_self_log_errors` prose at 1693). `crates/thegn-core/tests/config_example.rs`
fails if it is missing, and the runtime config-reference help page is generated
from this file — so this _is_ the user-facing documentation.

```toml
# Record an OSC 9 / OSC 777 raised hand ("Claude is waiting for your input") as
# an inbox row too. Off by default: a raised hand is live state — the sidebar
# dot, the ✋ chip and the "Needs you" ring already show it, and it clears when
# you answer — while agent CLIs emit one at the end of every turn, which buried
# every other notification. On, you get one CURRENT row per session (not one per
# turn) as an audit trail. Env: THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX.
agent_attention_inbox = false
```

### 8. Tests

**`crates/thegn-core/src/db_tests.rs`** — `db*.rs` is **not** in the justfile's
`cov_ignore` regex, so these methods are coverage-gated. Cover:

- upsert replaces rather than appends (raise twice for one session ⇒ one row,
  latest body and `since`);
- `list_session_attention` returns oldest-`since` first;
- `clear_session_attention` removes one session, leaves siblings;
- `clear_session_attention_for_worktree` removes every session on that worktree
  and returns the count;
- `clear_all_session_attention` empties;
- `prune_session_attention` drops only rows past the cutoff and returns the count;
- `del_worktree` cascades (assert the row is gone after removal);
- the migration ladder still reaches `SCHEMA_VERSION` — the existing ladder tests
  at `db_migrate.rs:640` / `:710` must stay green;
- the `ver < 57` cleanup: open a DB at an older `user_version` carrying unread
  `agent_attention` rows, reopen, assert they are read.

**`crates/thegn-core/src/config_tests.rs`** — mirror
`surface_self_log_errors_defaults_off_and_overlay_applies` (line 2444):

- `agent_attention_inbox` defaults to `false`;
- `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX=true` flips it (the env-overlay
  coverage test at `tests/env_overlay_coverage.rs` requires every knob to be
  _exercised_, not merely declared).

---

## Approach notes

- Storage + config only. No producer, no consumer, no behaviour change. If you
  find yourself editing `daemon/session.rs` or `attention_status.rs`, that is
  chunk 3.
- Every `let _ =` you add is a best-effort cache write on a disposable table —
  give each a `// best-effort:` comment so `test/ignored-result-ratchet.txt`
  does not need to grow (it is shrink-only).
- Keep the `db.rs` edit to DDL + version + the two `ver`-gated blocks. New logic
  belongs in `db_notification.rs`.

## Done criteria

- [ ] `just quick thegn-core` clean.
- [ ] `cargo nextest run -p thegn-core session_attention` and
      `cargo nextest run -p thegn-core -- config_tests` pass.
- [ ] `cargo nextest run -p thegn-core -- db_migrate` passes (ladder reaches 57).
- [ ] `cargo test -p thegn-core --test config_example` passes (the key is
      documented) and `--test env_overlay_coverage` passes (the knob exists **and**
      is exercised; `test/env-overlay-ratchet.txt` **must not gain a line**).
- [ ] `cargo test -p thegn-core --test hm_module_drift` passes.
- [ ] `thegn config validate --strict` accepts a config setting the new key.
- [ ] Opening an existing DB migrates cleanly and retires the old unread
      `agent_attention` backlog exactly once.
- [ ] Nothing observable changed yet — the daemon still writes the old row.
- [ ] Before push: `THEGN_ALLOW_HEAVY=1 just test` and
      `THEGN_ALLOW_HEAVY=1 just coverage` (core ≥95%).
