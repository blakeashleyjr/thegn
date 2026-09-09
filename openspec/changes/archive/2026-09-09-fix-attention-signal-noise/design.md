# Design — an OSC raised hand is live state, not an inbox event

## The state table

A raised hand has one value at a time, so it belongs in a state table keyed by
the thing that raised it. Schema v57:

```sql
-- v57: live "raised hand" state, one row per daemon session. An OSC 9 /
-- OSC 777;notify signal is LIVE STATE — deleted the moment the user answers —
-- not an inbox event (THE-68). This row is the cross-process channel from the
-- session actor to the compositor's attention scorer.
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

`session` is the primary key, so a re-raise **replaces** — the table is bounded
by the live session count, not by turns. `since` is unix **seconds**
(`thegn_core::util::now`), the honest moment the hand went up, which is what the
attention sort's longest-waiting-first tie-break and the "N ago" hint read.

## Lifecycle — every arm that lowers a hand

The table is pure cache. Losing it costs one stale-free hydration, nothing more;
what it must never do is keep a hand up after the demand is gone, because that is
the exact defect being fixed.

| Event                                                                               | Action                                   |
| ----------------------------------------------------------------------------------- | ---------------------------------------- |
| OSC 9 / 777 hit (`on_attention`)                                                    | upsert by `session`                      |
| user input reaches the child (`on_input`)                                           | delete by `session`                      |
| session actor ends (exit / kill)                                                    | delete by `session`                      |
| session registry created empty (daemon boot; host boot, `[daemon] enabled = false`) | `clear_all_session_attention`            |
| `del_worktree` / `del_worktrees_for_repo`                                           | cascade delete by `worktree_path`        |
| `mark_all_read` (`a`, `Alt-Shift-R`) and the per-worktree ack (`x`)                 | delete for those worktrees               |
| `Db::startup_prune`                                                                 | age sweep (7 days), beside the ack sweep |

Two of these are load-bearing rather than housekeeping:

- **The ack/clear-all arms.** If quieting a worktree retired only its inbox rows,
  the new live state would become a _new_ un-clearable nag — the same bug in a
  different table. The clear must cover both halves of one demand.
- **A signal from a session with no worktree writes no row.** An unattributed
  hand can light no sidebar row anyway; today it becomes an unattributed
  host-global inbox row. The live feed state is unchanged either way.

The age sweep is a table-growth bound only. A hand is lowered by an answer or a
session ending, never by the sweep: a raised hand outliving its session by a week
is a leak, not a demand.

## Why the demand reuses `AgentNeedsInput`

`AttentionInputs` gains one field and `score` gains one `consider` arm:

```rust
/// A live OSC 9 / OSC 777 raised hand for this worktree, carrying the moment it
/// went up. `None` when no hand is up.
pub attention_signal_since: Option<i64>,
```

```rust
if let Some(at) = inputs.attention_signal_since {
    consider(T::Blocked, 0, R::AgentNeedsInput, Some(at), 0);
}
```

That is not invented here. `AttentionInputs.stage_blocked_since`
(`attention.rs:455`, `:496`) is the same pattern, added for pipeline stages parked
on a human, with the comment "_the demand is identical … reusing the existing
reason keeps the closed reason set (and every surface that renders it)
unchanged_". "An agent is asking you something" is **one demand however it was
signalled**, so a signal-shaped tier would be a second name for a state the model
already has — and every surface that renders a reason (the dot, the `✋` chip,
the needs-you popup, the `Alt a` ring, the sort's tie-break) would need an arm
for it. Reusing the reason means those surfaces need **no** change at all, which
is why this change ships with zero render-path edits.

The `NotificationKind::AgentAttention` scoring arm stays exactly as it is: a
deliberate push is still a real event with a real row that still scores.

## Why the clear predicate was extracted

The display filter and the clear each carried their own copy of "does this row
belong to the active repo's inbox?", and they drifted: display fail-open, clear
fail-closed. The fix is one pure function in `thegn-core`, projected by both:

```rust
pub fn shows_in_repo_inbox(
    worktree_path: &str,
    repo_paths: &HashSet<String>,
    all_known: &HashSet<String>,
) -> bool {
    worktree_path.is_empty()
        || repo_paths.contains(worktree_path)
        || !all_known.contains(worktree_path)
}
```

and the clear becomes one statement with the same three arms:

```sql
UPDATE notifications SET read=1
 WHERE worktree_path = ''
    OR worktree_path IN (<repo placeholders>)
    OR worktree_path NOT IN (<known placeholders>)
```

Two empty-set edges, both deliberate and both tested: an empty `repo_paths`
drops that arm (SQLite rejects `IN ()`), and an empty `all_known` drops the
`NOT IN` arm so the statement marks everything read — which is the correct
fail-open answer, since a registry with no rows knows nothing and can attribute
nothing to another repo.

Fixing the clear by making it fail-**closed** to match the display was the
alternative and is wrong: those rows are shown deliberately (the repo's main
checkout never gets a `worktrees` row, and hiding its notifications would be a
worse bug than failing to clear them). The displayed set is the contract; the
clear follows it.

## Render damage channel, wake paths, schema

- **Damage channel: none added.** No render-path edit. The signal reaches the
  frame through `SidebarStatus`, which already marks chrome dirty, so the
  decision stays whatever `render_plan::plan` already returns for a hydration
  result. `render_plan` and its invariant tests are untouched.
- **Wake paths: none added.** No timer, no poll, no new channel. The daemon write
  happens on `spawn_blocking` off the byte funnel exactly as the notification
  write does today (the funnel must never block on SQLite); the host read is one
  small table read on the hydration worker beside `list_merge_queue` /
  `list_dispatches`. The 0%-idle contract is untouched.
- **Schema: `user_version` 56 → 57**, with a `ver < 57` one-time cleanup
  following the v46 `process_failed` precedent (`db.rs:846-857`): mark the unread
  `agent_attention` backlog read, once, gated on the pre-bump on-disk version. A
  deliberate push raised _after_ the upgrade is untouched. `SCHEMA_VERSION` bumps
  are a known cross-branch collision point — rebase and re-check the number
  immediately before landing.

## Help contexts

No new interactive surface, so no new help context key: the affected contexts are
the existing `panel:notifications` (`docs/help/panel.md`) and
`zone:statusbar` / `zone:masthead` (`docs/help/bars.md`), both of which gain
prose only. No new action id, chord, zone or panel section ⇒ no `ACTION_SPECS`
change, and none of `test/help-ratchet.txt`, `test/help-prose-ratchet.txt`,
`test/help-context-ratchet.txt` may gain a line. The config-reference page is
generated at runtime from `config/config.toml.example` — the new key is
documented there, never hand-written into a page.

## Invariants

| Invariant                                  | How this change honors it                                                                                                                                                                                      |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **0% idle**                                | No new wake source or timer; daemon write on `spawn_blocking`, host read on the existing hydration worker.                                                                                                     |
| **Render decision is pure**                | No render-path edits; the signal arrives via `SidebarStatus`, which already dirties chrome.                                                                                                                    |
| **`thegn-core` substrate-free, 95% lines** | New core code is a pure predicate module, a plain struct, one `AttentionInputs` field + one `consider` arm, and SQL. `db*.rs` is not `cov_ignore`d, so the new store methods carry `db_tests.rs` coverage.     |
| **git is truth; the DB is a cache**        | `session_attention` is derived and disposable — reaped on answer, session end, daemon boot, `del_worktree`, and by the age sweep.                                                                              |
| **Ignored `Result`s are deliberate**       | Every new `let _ =` is a best-effort cache write carrying a `// best-effort:` comment; `test/ignored-result-ratchet.txt` must not grow.                                                                        |
| **Config gate**                            | The new key is documented in `config/config.toml.example` and has a real `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX` override — `test/env-overlay-ratchet.txt` is shrink-only, so pinning it is not an option. |
| **Keep god-files from growing**            | New logic lands in `notification_scope.rs` / `db_notification.rs`; the `db.rs` edit is DDL, version and the two `ver`-gated blocks only.                                                                       |
| **Help ratchets**                          | Prose-only doc edits; no new action, chord, zone or panel section.                                                                                                                                             |
| **e2e**                                    | Chrome pixels unchanged by design (dot/chip/ring fire on the same evidence). A moved snapshot means the chrome changed — understand why before re-recording.                                                   |

## Open questions

- The opt-in inbox path (`agent_attention_inbox = true`) deliberately bypasses
  `notify::record`, because the daemon may be a separate process with no
  `NotifyState`. Routing an audit row through the rules engine would be better,
  and needs a daemon→host notify hop that does not exist yet. Deferred.
- `put_notification("disk_cleaned", …)` renders as "status changed ⟳" because
  the kind string is not in `NotificationKind` — real, adjacent inbox noise, but
  a 27th kind is a different blast radius. Follow-up issue, not folded in here.
