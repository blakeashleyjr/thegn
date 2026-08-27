# THE-68 — security / test / bug review

**Verdict: PASS** (ready for the merge queue), with three defects found and
fixed on the lane, and four non-blocking observations recorded below.

Branch `tg/the-68-log-noise`, reviewed at `bb894177` (architect: APPROVED),
plus the three review commits described in §1.

---

## 0. What was reviewed

The whole `main...HEAD` diff, adversarially, against:

- swallowed errors / `let _ =` without a reason
- SQL injection, path handling, permission and untrusted-input surfaces
- cross-process and cross-task race conditions
- missing tests on the failure paths
- architecture ratchets and the schema-migration gate

Gates actually run (scoped, per the dev-loop policy — the heavy full-workspace
gates are the pre-push/CI job, not this one):

| gate | result |
| --- | --- |
| `cargo nextest -p thegn-core` (attention, db_tests, config_tests, notification_scope) | 60/60 pass |
| `cargo nextest -p thegn-core` (env_overlay, surface_gaps, config-example coverage) | 14/14 pass |
| `cargo nextest -p thegn-host daemon::session` | 15/15 pass, ×8 consecutive runs |
| `cargo clippy -p thegn-host --all-targets -- -D warnings` | clean |
| `test/ratchet.sh ignored-result … crates` | clean (323 pinned, no growth) |
| `just openspec-validate` | 167/167 pass |

Not run here (CI-only, machine-heavy): `just coverage`, `just test-doc`,
`check-cross`, `just e2e`, full-workspace `just test`.

---

## 1. Defects found and fixed on this lane

### 1.1 The raise and the lower are unordered `spawn_blocking` tasks — an answer could be overtaken (fixed)

`SessionActor::on_attention` spawned a blocking task that INSERTs the
`session_attention` row; `on_input` → `clear_attention_row` spawned a separate
blocking task that DELETEs it. `tokio::task::spawn_blocking` guarantees no
ordering between tasks, and both contend for the same `db` mutex, so whichever
grabbed the lock first won.

**Failure scenario.** An agent emits its end-of-turn `OSC 9` and the user's
keystroke reaches the pane in the same instant (queued input, a pasted answer, a
scripted driver). The DELETE runs first and removes nothing; the INSERT then
lands. `self.attention` is `None` — the pane is not blocked, the feed says so —
but `session_attention` holds a hand nobody is waiting behind, so
`attention_status` scores the worktree `Blocked`/`AgentNeedsInput` on every
hydration and the sidebar dot, the `✋` chip and the needs-you ring all light for
a question that was already answered. It heals only on the next answered turn,
or by a manual clear. That is precisely the un-clearable nag THE-68 exists to
remove, reintroduced through the new state table.

**Fix** (`fix(notify): a raise overtaken by its own answer must not write`):
`SessionActor::attention_gen`, an `AtomicU64` bumped **in order on the actor
loop** — once when the hand goes up, once inside `clear_attention_row` before
the delete is queued. The upsert task carries the generation it was spawned with
and, under the DB lock, declines the write if the counter has moved past it.
Because both bumps happen synchronously on the single actor task, the ordering
is total regardless of how the blocking pool schedules the two writes. The
opt-in audit row is deliberately left unguarded: the agent *did* ask, and that
trail is meant to record the ask rather than the pending state.

### 1.2 The opt-in audit trail grows one row per turn again after any clear (fixed)

`crates/thegn-host/src/daemon/session.rs` — the `agent_attention_inbox` path
retired the session's previous row by scanning `get_unread_notifications()`.
The accompanying comment states the reason the retire must DELETE rather than
mark read: *"the inbox lists read rows too (`get_all_notifications`)"*. The
sweep contradicted its own comment by only ever looking at unread rows.

**Failure scenario.** `[notifications] agent_attention_inbox = true`. Turn 1
writes row A. The user presses `x` on it, or `a` (clear all) — A is now read.
Turn 2's sweep scans only unread rows, does not find A, does not delete it, and
inserts row B. The inbox now lists A *and* B. Every subsequent clear buys
another permanent row: exactly the "one row per agent turn, forever" pile
THE-68 reported, one clear-all later.

**Fix** (`fix(notify): the opt-in retire sweep must see read rows too`):
`get_all_notifications(usize::MAX)` (which `notifications_query` treats as "no
cap", so no SQL `LIMIT` is emitted). New regression test
`a_read_audit_row_is_still_retired_by_the_next_turn` marks the first turn's row
read between the two signals — asserting first that the second turn has *not*
yet landed, so the test cannot pass vacuously — and requires exactly one total
row afterwards. It fails on the old code (the read row is never matched, so the
total is 2).

### 1.3 The named flaky test — a lost-frame race in the harness, not a timing tolerance (fixed)

**Specific check requested:** `daemon::session::tests::an_osc_attention_signal_blocks_and_input_clears_it`.

It **still holds under the new state-table flow** — 15/15 across 8 consecutive
runs, and the two behaviours it pins (the feed reaching `blocked`, stdin
clearing it) are untouched by the rewrite; the state-table writes ride
`spawn_blocking` and never gate the feed.

It does **not** need the load-tolerance / real-jiffies treatment. The flake is
structural, and no timing constant fixes it. `spawn_actor*` calls
`pane_pty::open_pty` — which **starts the child immediately** — and then
`tokio::spawn(actor.run(…))`, all before returning. The tests then subscribed on
the next line:

```rust
let h = spawn_actor_as(r"printf '\033]9;pick a branch\007'; cat", None, "claude");
let mut feed = h.events.subscribe();   // ← too late if the actor already ran
```

`tokio::sync::broadcast` delivers nothing sent before a subscribe. If the test
thread is descheduled between `spawn_actor_as` returning and `subscribe()` — the
window that widens exactly under load, which is when this was reported — a
worker thread can drain the already-queued OSC bytes and publish the `blocked`
frame into a channel with no receiver. The frame is gone permanently:
`await_state` then burns its full 10s deadline and the test fails on
`expect("OSC 9 must raise a blocked state")`. Raising the deadline cannot help,
because the event was never buffered. `tombstone_is_buried_before_the_exit_event`
(`echo last-words; exit 3`) and `an_osc_signal_writes_state_not_an_inbox_row`
had the same shape.

**Fix** (`test(daemon): subscribe to the session feed before the actor can publish`):
`Harness` now carries a `feed` receiver subscribed **before**
`tokio::spawn(actor.run(…))`, and the three affected tests read it instead of
subscribing after the fact. The race is closed by construction rather than
papered over. The now-unused `Harness::events` sender was removed (it would have
tripped `dead_code` under `-D warnings`).

---

## 2. Non-blocking observations

These are recorded, not required for the merge queue.

1. **`session_attention.title` / `body` are write-only.** The only production
   reader, `attention_status::collect_attention`, uses `worktree_path` and
   `since`. With the default config, the agent's actual question text
   ("pick a branch") is therefore not user-visible anywhere — the live feed
   message reaches only `daemon::service`'s `wait`, which discards it, and the
   sidebar's blocked dot is derived from `thegn_core::attention`, not from the
   OSC message. That is consistent with THE-68's ask ("drop those by default"),
   and the columns are the obvious substrate for a future hover/detail surface,
   but today they are dead state carrying untrusted process output.

2. **`mark_notifications_read_scoped` re-implements `shows_in_repo_inbox` in
   SQL.** The change's whole thesis is "one predicate, no second copy to
   drift" — and the display filter does project the function, but the clear
   hand-writes the same three arms as SQL. Both sides are tested separately;
   nothing pins them to each other. A table-driven test that runs a corpus
   through the SQL clear and through `shows_in_repo_inbox` and asserts the two
   sets are equal would make the claim structural.

3. **CHANGELOG overstates the v57 sweep's precision.** It says deliberate pushes
   (`thegn notify push --urgency alert`, control API, MCP) "are untouched". Those
   map to `kind = 'agent_attention'` (`daemon/service.rs:1115`), so any that were
   *already unread* at upgrade time are marked read by the one-time sweep. The
   in-code comment is accurate ("raised **after** the upgrade is untouched"); the
   CHANGELOG line reads broader than the code. Cosmetic, and the blast radius is
   one already-delivered notification.

4. **`clear_stale_raised_hands` is keyed on this process's daemon route, not on
   whether a daemon exists.** `daemon_active` is false for `THEGN_NO_DAEMON=1`,
   `THEGN_BENCH_FIRST_FRAME_EXIT`, and an over-long socket path, so a host that
   degraded to in-process panes will empty the shared table even if another
   instance's daemon has live hands in it. Best-effort by design, self-healing on
   the agent's next turn, and the alternative (leaving genuinely orphaned rows to
   nag forever) is worse. Noted so it is a known trade rather than a surprise.

---

## 3. Things checked and found sound

- **No SQL injection.** The one dynamically-built statement
  (`mark_notifications_read_scoped`) interpolates only `?` placeholders derived
  from slice lengths and binds every value through `params_from_iter`; the bind
  order (`repo_paths` then `all_known`) matches the arm order, and each arm is
  omitted when its slice is empty because SQLite rejects `IN ()`. The
  empty-`all_known` early return is correct, not a fail-open hole: an empty
  registry can attribute no row to another repo, so the display filter shows
  everything and the clear must match. All four boundary cases have tests.
- **The v57 migration is correctly gated and ordered.** It reads the pre-bump
  on-disk `user_version`, and the stamp is written last, so a crash mid-migration
  re-runs the idempotent steps rather than skipping them.
  `v57_retires_the_unread_agent_attention_backlog_once` pins both the one-shot
  behaviour and that a post-upgrade row survives.
- **Every delete path cascades.** `del_worktree` and `del_worktrees_for_repo`
  both drop the worktree's hands, and in `del_worktrees_for_repo` the
  `session_attention` delete is sequenced *before* `DELETE FROM worktrees`, so
  its subquery still resolves. Both have tests.
- **Both ack paths lower the hand.** `actions.rs`'s per-row `x` and
  `handlers/attention.rs`'s `a` (scoped, unscoped, and the acked-set loop that
  covers fail-open paths outside `repo_paths`) all call
  `clear_session_attention_for_worktree` / `clear_all_session_attention`, so the
  new state cannot become a new un-clearable nag.
- **The ignored `Result`s are sanctioned.** Every new `let _ =` is a
  disposable-cache write annotated in place, and
  `test/ratchet.sh ignored-result` reports no growth (323 pinned).
- **No new `#[cfg]`, colour/glyph literal, `gh` call, or `async fn` in a
  provider trait** — the other ratchets are untouched. `openspec validate
  --all --strict` is green, and the two delta specs match the implementation
  (including the "a session with no worktree records nothing, feed state
  unchanged" arm, which the early return honours).
- **No new untrusted-input surface.** The OSC body was already bounded by
  `OSC_MAX_PAYLOAD` before this change and is now stored rather than rendered.
  No subprocess, path, or permission handling was added.
- **The performance shape holds.** The hydration worker gains one small indexed
  table read beside `list_merge_queue` / `list_dispatches`; the boot-time clear
  runs off-loop on a `Qos::Background` thread; nothing new touches the render
  path or the idle loop.

---

## 4. Merge

Ready for `thegn integrate`. The merge step is not run by this review.
