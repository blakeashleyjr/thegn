# THE-68 — "Claude is waiting on your input" noise, and clear-all that doesn't clear

**Branch:** `tg/the-68-log-noise`
**Issue:** THE-68 — _"Claude is waiting on your input" type logs are polluting the
log. Drop those by default. Also clearing all notifications didn't actually
clear them all for me._

Two symptoms. They turn out to share one producer, and each has its own root
cause. This document establishes both, then specifies the change.

---

## 1. What "the log" is, and where the rows come from

The "log" is the **notification inbox** (System ▸ Notifications, the unified
inbox overlay, and the `⚑`/`✉` chips) — not `thegn.log`, which gets nothing here
(`daemon/session.rs:742` logs the signal at `debug!`, and no sink is installed
unless `THEGN_LOG` is set).

The producer is the OSC attention scanner. Every PTY byte funnels through
`SessionActor::on_output` (`crates/thegn-host/src/daemon/session.rs:449`), which
feeds `thegn_core::osc_attention::OscAttentionScanner`. A hit calls
`on_attention` (`session.rs:741`), which does two things:

```rust
self.attention = Some(sig);        // live state — cleared by on_input
self.publish_state();              // edge-triggered event on the feed
// ...and then, off-thread:
db.put_notification("agent_attention", &source, &message, &worktree)
```

Claude Code emits `OSC 9` / `OSC 777;notify` **at the end of every turn** — the
literal body is "Claude is waiting for your input". So the append-only
`notifications` table gains one row per turn, per agent, forever. With a handful
of agents that is hundreds of rows a day, and they are the only thing in the
inbox. That is symptom 1.

The comment at `session.rs:754-757` explains why the row exists:

> Persist it so the compositor's sidebar lights up through the path it already
> has (an unread `agent_attention` row is what raises the attention tier).

That is true — `attention_status.rs:149` folds unread rows into
`AttentionInputs.unread`, and `attention.rs:484` maps `AgentAttention` to
`(Blocked, AgentNeedsInput)`. **The notification row is being used as a
cross-process channel for live state.** Every defect below follows from that.

### The three consequences of using an append-only log as a state channel

1. **Unbounded rows.** Live state has one value; a log has one row per edge.
2. **It never clears.** `on_input` clears the live `self.attention`, but the DB
   row stays unread — so the worktree scores `Blocked` until the user marks the
   row read by hand. `openspec/changes/add-osc-attention-signaling/specs/attention-signals/spec.md`
   _specifies_ the opposite ("**Scenario: Resume clears the signal**"). The
   persisted half violates the spec the live half satisfies.
3. **It bypasses routing.** `notify::record` (`crates/thegn-host/src/notify.rs:286`)
   is the one funnel where `[[notifications.rules]]`, DND, debounce, toast, sound
   and push are decided. The daemon calls `put_notification` directly, so a user
   cannot mute this with a rule, and the burst-suppression that already exists
   for `process_failed` (`notify.rs:297`) never applies.

---

## 2. Why "clear all" leaves rows behind

Two functions decide what belongs to the active repo's inbox. They disagree.

**Display** — `hydrate_feed::populate_notifications` (`hydrate_feed.rs:99-107`)
is **fail-open**, and says so:

```rust
// Scope FAIL-OPEN on the registry: a row tagged with a path the DB
// doesn't know (the repo's main checkout, an externally-created
// worktree — neither gets a `worktrees` row) is kept, not hidden.
notifications.retain(|n| {
    n.worktree_path.is_empty()
        || repo_paths.contains(&n.worktree_path)
        || !all_known.contains(&n.worktree_path)      // ← the fail-open arm
});
```

**Clear** — `handlers::attention::mark_all_read` (`handlers/attention.rs:284`) →
`Db::mark_notifications_read_scoped` (`db_notification.rs:96-107`) is
**fail-closed**:

```sql
UPDATE notifications SET read=1 WHERE worktree_path='';
UPDATE notifications SET read=1 WHERE worktree_path=?1;   -- once per repo_path
```

So the set `{rows tagged with a path the worktrees registry does not know}` is
**displayed but never cleared**. Press `a`; `mark_read_where(|_| true)`
(`handlers/attention.rs:307`) optimistically greys them in the model, the next
hydration re-reads the DB, and they come back unread. That is symptom 2, exactly
as reported: _"didn't actually clear them **all**"_.

Which rows land in that set? Precisely the ones the main checkout produces —
the repo's own main worktree has no `worktrees` row — plus externally-created
worktrees, renamed paths, and any producer whose path spelling differs from the
registry's. **The OSC producer writes `self.meta.worktree` verbatim**, so
symptom 1 and symptom 2 meet in the same rows: the un-clearable pile is the
"Claude is waiting" pile.

The two symptoms are independent bugs; fixing either alone leaves the other.

---

## 3. Thesis

> An **inbox row is an event you might otherwise miss**. A **raised hand is live
> state you can already see.** Ambient OSC chatter is the second kind; a
> deliberate push (`thegn notify push`, `notify.push` over the control API, MCP
> `request_human`) is the first.

Two changes follow, one per symptom:

**D1 — An OSC attention signal is live session state, not an inbox event.**
The `agent_attention` _kind_ stays exactly as it is for deliberate pushes
(`daemon/service.rs:1112` maps `urgency = alert` onto it; `attention.rs:484`
keeps its arm). What changes is that the OSC scanner stops appending to the log
and instead upserts a row in a small **`session_attention` state table**, keyed
by session, deleted when the user answers. `attention_status` reads that table
the same way it already reads `merge_queue` and the pipeline roster, and feeds
`AttentionInputs.attention_signal_since`, which scores through the **existing**
`(Blocked, AgentNeedsInput)` evidence.

That last move is not invented here: `AttentionInputs.stage_blocked_since`
(`attention.rs:455`, `attention.rs:496`) is the same pattern, added for pipeline
stages parked on a human, with the comment "_the demand is identical … reusing
the existing reason keeps the closed reason set (and every surface that renders
it) unchanged_". We follow it verbatim — **no new tier, no new reason, no new
notification kind, no new surface.**

The behaviour the user gets: the sidebar dot, the `✋` chip, the "Needs you"
popup and the `Alt a` ring all work exactly as today; the inbox stops filling;
and answering the agent now clears the demand, which is what the spec always
said and what the code never did.

A `[notifications] agent_attention_inbox` knob (**default `false`**) restores the
inbox row for anyone who wants the audit trail — the same shape as the existing
`surface_self_log_errors` dev flag in the same section. When on, the write is a
delete-then-insert per session, so it is _one current row per session_, not one
per turn.

**D2 — "Clear all" clears exactly what the inbox displays.**
Extract the display predicate into one pure, tested function in `thegn-core` and
make both callers use it. The clear becomes a single statement with the same
three arms:

```sql
UPDATE notifications SET read=1
 WHERE worktree_path = ''
    OR worktree_path IN (repo_paths…)
    OR worktree_path NOT IN (all_known…)
```

One predicate, two call sites, one behaviour. The asymmetry cannot come back
because there is no second copy to drift.

---

## 4. Invariants this change must respect

Checked against `CLAUDE.md` and `docs/ARCHITECTURE.md`; each is a done-criterion
in the chunk that touches it.

| Invariant                                               | How this change honors it                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **0% idle** (ARCH §2)                                   | No new wake source, no timer. The daemon write happens on `spawn_blocking` off the byte funnel (as today); the host read happens on the hydration worker beside `list_merge_queue`/`list_dispatches`. Nothing touches the loop.                                                                                                                                                                                |
| **Render decision is pure** (ARCH §3)                   | No render-path edits. The signal reaches the frame through `SidebarStatus`, which already marks chrome dirty. `render_plan` untouched.                                                                                                                                                                                                                                                                         |
| **`thegn-core` is substrate-free, 95% lines** (ARCH §1) | New core code is a pure predicate module, a plain struct, an `AttentionInputs` field + one `consider` arm, and SQL in `db_notification.rs`. `db*.rs` is **not** in the justfile `cov_ignore` list, so the new store methods need `db_tests.rs` coverage.                                                                                                                                                       |
| **git is truth; the DB is a cache** (ARCH §9)           | `session_attention` is derived, disposable state: reaped on answer, on session end, on daemon boot (an empty session registry means no raised hands), on `del_worktree`, and by an age sweep in `startup_prune`. Losing the table costs one stale-free hydration, nothing more.                                                                                                                                |
| **Ignored `Result`s are deliberate**                    | Every new `let _ =` is a best-effort cache write and carries a `// best-effort:` comment. `test/ignored-result-ratchet.txt` must not grow.                                                                                                                                                                                                                                                                     |
| **Config gate** (ARCH §7)                               | A new `[notifications]` shallow key needs a line in `config/config.toml.example` (`tests/config_example.rs`) **and** a real `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX` knob in `Config::env_overlay` — `test/env-overlay-ratchet.txt` is **SHRINK-ONLY**, so pinning it there instead is not an option even though all six existing `notifications.*` keys are pinned. Also check `tests/hm_module_drift.rs`. |
| **Schema ladder** (ARCH §9)                             | `SCHEMA_VERSION` 56 → 57 with a `ver < 57` one-time cleanup, following the v46 `process_failed` precedent at `db.rs:846-857`. `SCHEMA_VERSION` bumps are a known merge-conflict point — rebase before landing.                                                                                                                                                                                                 |
| **Keep god-files from growing**                         | New core logic goes in a new sibling module, not into `config.rs`/`db.rs`/`run.rs`. The `db.rs` edit is DDL + version only.                                                                                                                                                                                                                                                                                    |
| **Help ratchets**                                       | No new action id, chord, zone or panel section ⇒ no `ACTION_SPECS` change and no help-ratchet churn. The config-reference help page is generated at runtime — never hand-written.                                                                                                                                                                                                                              |
| **e2e**                                                 | Chrome pixels are unchanged by design (the dot/chip/ring keep firing on the same evidence). If a snapshot moves, re-record with `just e2e-update` and review — do not paper over it.                                                                                                                                                                                                                           |

---

## 5. Contracts fixed here (so chunks can be written in parallel)

Chunk 3 is written against these signatures without waiting for chunk 2's code.

```rust
// crates/thegn-core/src/osc_attention.rs
/// A raised hand that is still up: one row per daemon session, deleted when the
/// user answers. Live state, NOT an inbox event — see THE-68.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttention {
    pub session: String,
    pub worktree_path: String,
    pub title: String,
    pub body: String,
    /// Unix **seconds** (`thegn_core::util::now`), the honest `since`.
    pub since: i64,
}

// crates/thegn-core/src/store/notification.rs — added to `NotificationStore`
fn put_session_attention(&self, a: &crate::osc_attention::SessionAttention) -> Result<()>;
fn clear_session_attention(&self, session: &str) -> Result<()>;
fn clear_session_attention_for_worktree(&self, worktree_path: &str) -> Result<usize>;
fn clear_all_session_attention(&self) -> Result<()>;
fn list_session_attention(&self) -> Result<Vec<crate::osc_attention::SessionAttention>>;

// crates/thegn-core/src/notification_scope.rs  (new module, pure)
pub fn shows_in_repo_inbox(
    worktree_path: &str,
    repo_paths: &std::collections::HashSet<String>,
    all_known: &std::collections::HashSet<String>,
) -> bool;

// crates/thegn-core/src/store/notification.rs — CHANGED signature
fn mark_notifications_read_scoped(
    &self,
    repo_paths: &[String],
    all_known: &[String],
) -> Result<()>;

// crates/thegn-core/src/attention.rs — added to `AttentionInputs`
/// A live OSC 9 / OSC 777 raised hand for this worktree, carrying the moment it
/// was raised. Same demand as an `AgentAttention` notification, so it scores
/// through the EXISTING `AgentNeedsInput` blocked evidence — mirrors
/// `stage_blocked_since`. `None` when no hand is up.
pub attention_signal_since: Option<i64>,

// crates/thegn-core/src/config.rs — added to `NotificationsConfig`
/// Also record an OSC raised hand as an inbox row (audit trail). Off by
/// default: the raised hand is live state, already shown by the sidebar dot and
/// the ✋ chip, and one row per agent turn buried every other notification.
pub agent_attention_inbox: bool,   // default false
```

Schema (chunk 2 owns the DDL):

```sql
-- v57: live "raised hand" state, one row per daemon session.
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

### Lifecycle of a `session_attention` row (chunk 3 implements every arm)

| Event                                                                                      | Action                                                                                |
| ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------- |
| OSC 9 / 777 hit (`on_attention`)                                                           | upsert by `session`                                                                   |
| user input reaches the child (`on_input`)                                                  | delete by `session`                                                                   |
| session actor ends (exit / kill)                                                           | delete by `session`                                                                   |
| session registry created empty (daemon boot, or host boot with `[daemon] enabled = false`) | `clear_all_session_attention`                                                         |
| `del_worktree`                                                                             | cascade delete by `worktree_path` (chunk 2)                                           |
| `mark_all_read` (`a`, `Alt Shift R`) and per-worktree ack (`x`)                            | delete for those worktrees — otherwise the new state becomes a _new_ un-clearable nag |
| `Db::startup_prune`                                                                        | age sweep (7 days), like the existing ack sweep                                       |

A signal from a session with **no** worktree writes no row (it could not light
any sidebar row anyway) — the live feed state is unchanged. Today such a signal
becomes an unattributed host-global inbox row.

---

## 6. Chunks and land order

| #   | Title                                                      | Depends on             | Overlap                         |
| --- | ---------------------------------------------------------- | ---------------------- | ------------------------------- |
| 1   | "Clear all" clears exactly what the inbox shows            | —                      | `handlers/attention.rs` with #3 |
| 2   | Core: `session_attention` state table + config knob        | —                      | none                            |
| 3   | Wire the live signal: daemon writes state, scorer reads it | #2's API (fixed in §5) | `handlers/attention.rs` with #1 |
| 4   | Specs, help prose, changelog                               | — (reads §1–§3)        | none                            |

**Land order: 1 → 2 → 3 → 4.** Chunks 1, 2 and 4 are mutually independent and
can be worked at the same time. Chunk 3 compiles only once chunk 2 is in; it
also adds ~2 lines to `mark_all_read` in `handlers/attention.rs`, the one
function chunk 1 also edits — chunk 3 rebases onto chunk 1 rather than the
reverse (chunk 1 changes a call's arguments, chunk 3 adds a new call beside it;
the conflict, if any, is trivial and both anchors are named in the chunk files).

---

## 7. Found while investigating — deliberately NOT in scope

Flagged so it is a decision rather than an oversight:

**`put_notification("disk_cleaned", …)` at `hydrate.rs:3596` uses a kind string
that is not in `NotificationKind`.** `Db::notifications_query` (`db.rs:978`)
falls back to `StatusChanged` for any unparseable kind, so a reclaimed-disk
notice renders in the inbox as "**status changed** ⟳" and counts toward the
neutral unread badge. It is genuinely mislabelled inbox noise, so it is adjacent
to this issue — but fixing it means adding a 27th `NotificationKind`, which
touches the pinned `ALL` count, the `default_priority` totality test, the
`hued_glyph` table, and the `config.toml.example` prose test. That is a
different change with a different blast radius. **Recommend a follow-up issue**;
do not fold it in here.

---

## 8. Verification

Per the dev-loop policy: iterate with `just quick <crate>` and the specific
tests; run the heavy gates once at the end.

```sh
just quick thegn-core && just quick thegn-host      # per edit
cargo nextest run -p thegn-core notification        # per chunk
cargo nextest run -p thegn-core attention
cargo nextest run -p thegn-host attention
THEGN_ALLOW_HEAVY=1 just test                       # once, before push
THEGN_ALLOW_HEAVY=1 just coverage                   # core ≥95%, once
just openspec-validate
```

Manual check that closes the issue, in a scratch state dir so a live thegn is
not disturbed (`just start name=the68`):

1. In a pane, `printf '\033]9;Claude is waiting for your input\007'`.
2. The sidebar dot goes red / the `✋` chip counts the worktree — **and the
   inbox stays empty**.
3. Type into the pane. The demand clears without touching the inbox.
4. `thegn notify push --urgency alert …` still lands one inbox row (deliberate
   pushes are unchanged).
5. With rows tagged to the **main checkout** in the inbox, press `a`. They go
   read and **stay** read across a rehydrate.
