# Design: sidebar attention sort

## Urgency model (core, pure, coverage-gated)

`thegn_core::attention` — all branching lives here; the host only maps
model/DB state into `AttentionInputs`.

| Tier | Name    | Signals                                                                                                                                                                                           | Reasons                                      |
| ---- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| T0   | Blocked | unread `agent_attention`; MQ `needs_human`                                                                                                                                                        | agent needs input / merge queue needs you    |
| T1   | Failure | unread `agent_failed`/`test_failed`/`process_failed`/`log_error` (worktree-scoped); PR checks failed / CONFLICTING / CHANGES_REQUESTED; MQ `gate_failed`/`deferred`; latest cached CI run failing | agent failed, CI failed, PR has conflicts, … |
| T2   | Waiting | activity dot `waiting` (unread); unread `agent_done`; `read` sub-ranks below unread                                                                                                               | agent finished / still waiting               |
| T3   | Ready   | PR approved + checks green + MERGEABLE + non-draft; MQ `ready`                                                                                                                                    | ready to land                                |
| T4   | Working | activity `active`/loading; PR checks pending / CI running; MQ queued/folding/verifying/agent_running                                                                                              | working, CI running, integrating             |
| T5   | Idle    | everything else; **dirty is a within-idle sub-rank, never a tier**                                                                                                                                | —                                            |

- `needs_user()` = tier ≤ T2 → chip count + jump set.
- `sort_key()` = `(tier, sub, since or i64::MAX)` — **longest-waiting first**
  (starvation-proof).
- `since` honesty: notifications → `created_at_ms` (epoch seconds, real event
  time); merge queue → `updated_at` (real); activity → `quiet_since`/
  `busy_since` (real, via the new `activity::read_entries()` accessor);
  PR-derived → `None` (`pr_cache.fetched_at` is fetch time, not event time —
  those signals tie-break by home/position instead).

## Hysteresis (never reshuffle on churn)

`stable_order(prev, fresh)`: adopt the fresh order only when membership or
some worktree's **tier** changed; otherwise return the previous order
verbatim. The memo is a process-global `OnceLock<Mutex<Vec<(path, tier)>>>`
in `attention_status.rs` (the `glyph_cache()` pattern). Consequences:

- PR/CI `fetched_at` ticks, `busy_since` streak resets, and sub-rank churn
  never move rows.
- A real state change (tier transition, worktree added/removed) adopts the
  fresh urgency order in one repaint.
- Workspace bubbling needs no memo: it stable-sorts by tier only, so equal
  tiers keep manual order by construction.

## Data flow

`collect_sidebar_status` (hydration thread, Model tick ~5s) →
`attention_status::collect_attention`: one `get_unread_notifications()`, one
`list_pr_cache()` (new CacheStore method; parse `PrStatus`, `recompute_checks`
because `checks` is skip_deserializing), one `list_merge_queue()`, per-path
`get_ci_cache` + the activity snapshot → `score()` per path →
`SidebarStatus.{attention, attention_ranks, workspace_attention}` (all
`PartialEq`, so the status diff gates the repaint). `build_rows` denormalizes
scores onto rows (worktree = own score, workspace = rollup) and
`sort_groups(Attention)` orders by rank with home/gi as the unranked fallback.

Keys: `SidebarStatus.activity` is tab-name-keyed; everything attention is
**path-keyed** (the tab↔path join happens once, in hydration, via
`activity::read_entries()` which is path-keyed).

## Accepted staleness (v1)

- `pr_cache`/`ci_runs_cache` are written for the _active_ worktree only —
  PR/CI tiers for background worktrees are last-known-good. Fanning fetches
  across worktrees is future work.
- The mid-creation `Loading` overlay is loop-side state; hydration briefly
  scores such rows idle. Cosmetic (the ↻ dot still renders).

## Migration & defaults

- `SortMode::Activity` → `Attention`; `from_str("activity" | "attention")` →
  Attention is the entire ui_state migration (the persisted string is only
  read through `from_str`).
- `#[default]` moves to Attention: sessions without a saved `sort_mode` row
  adopt it; saved rows keep their mode. With no hydrated ranks yet the
  Attention arm degrades to manual order (no launch flash).
- Manual moves under Attention flip to Manual (existing `sidebar_reorder`
  behavior, inherited).
