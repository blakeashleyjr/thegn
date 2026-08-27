# Chunk 3 — Wire the live signal: the daemon writes state, the scorer reads it

**Issue:** THE-68 (first half, wiring). **Branch:** `tg/the-68-log-noise`.
**Depends on:** chunk 2's API (fixed verbatim in `.thegn/pipeline/architect/design.md` §5 —
you can write against it before chunk 2 lands, but it compiles only after).
**Land order:** third. **Overlaps:** adds ~2 lines to `mark_all_read` in
`crates/thegn-host/src/handlers/attention.rs`, which chunk 1 also edits — rebase
onto chunk 1; the anchors are named below and the two edits are adjacent, not
conflicting.

Read `.thegn/pipeline/architect/design.md` §1, §3, §4 and §5 first.

---

## What changes

Today `SessionActor::on_attention` (`crates/thegn-host/src/daemon/session.rs:741`)
appends an `agent_attention` **notification** per OSC hit, and
`attention_status.rs:149` folds that unread row into `AttentionInputs.unread`,
where `attention.rs:484` scores it `(Blocked, AgentNeedsInput)`. The row is being
used as a cross-process channel for live state: it accumulates one per agent
turn, it never clears when the user answers (`on_input` clears only the in-memory
copy), and it bypasses `notify::record`'s routing.

After this chunk the OSC path upserts a `session_attention` row instead, the
scorer reads that table, and the demand clears when the hand goes down. **The
`AgentAttention` notification kind and its scoring arm stay exactly as they
are** — a deliberate push (`thegn notify push`, `notify.push` over the control
API at `daemon/service.rs:1112`, MCP) is a real event and keeps its inbox row.

No new tier, no new reason, no new notification kind, no new surface: the signal
scores through the **existing** `AttentionReason::AgentNeedsInput`, exactly as
`AttentionInputs.stage_blocked_since` already does for pipeline stages
(`attention.rs:455` and `:496` — read those two comments; this is the same move
and should read the same way).

---

## Files

### 1. `crates/thegn-core/src/attention.rs` — one input, one arm

Add to `AttentionInputs` (after `stage_blocked_since`, line ~455):

```rust
    /// A live OSC 9 / OSC 777 raised hand for this worktree, carrying the
    /// moment it went up. Same demand an `AgentAttention` notification makes,
    /// so it scores through the EXISTING [`AttentionReason::AgentNeedsInput`]
    /// blocked evidence rather than inventing a signal-shaped tier — the
    /// sidebar's red dot, the ✋ chip and the needs-you ring then cover it with
    /// no new state anywhere. `None` when no hand is up. Mirrors
    /// [`Self::stage_blocked_since`].
    pub attention_signal_since: Option<i64>,
```

In `score`, immediately after the `stage_blocked_since` block (line ~496):

```rust
    // A live raised hand. Same tier/sub/reason as the two above: "an agent is
    // asking you something" is one demand however it was signalled.
    if let Some(at) = inputs.attention_signal_since {
        consider(T::Blocked, 0, R::AgentNeedsInput, Some(at), 0);
    }
```

**Tests** in the same module, modelled on
`stage_waiting_human_scores_as_blocked_through_the_existing_reason` (line 745):

- a raised hand alone scores `(Blocked, AgentNeedsInput)` with the given `since`
  and `needs_user()`;
- it does not invent a reason — assert the reason equals the notification path's;
- `None` scores nothing (a worktree with no other signal stays `Idle`);
- two worktrees both blocked sort longest-waiting first (extend or mirror
  `longest_waiting_first_within_tier`, line 974).

### 2. `crates/thegn-host/src/daemon/session.rs` — the producer

**`on_attention` (line 741).** Keep the `debug!`, the `self.attention = Some(sig)`
and the `publish_state()`. Replace the `put_notification` block:

```rust
        // A raised hand is LIVE STATE, not an inbox event: upsert it in
        // `session_attention` (one row per session, deleted the moment the user
        // answers) instead of appending an `agent_attention` notification per
        // agent turn. Claude Code and friends emit one at the end of EVERY
        // turn, so the old write filled the inbox with rows that no "clear all"
        // could retire and that kept the worktree Blocked after it was answered
        // (THE-68). The sidebar still lights up — `attention_status` reads this
        // table into the same `AgentNeedsInput` evidence.
        //
        // A session with no worktree writes nothing: an unattributed hand can
        // light no sidebar row. The live feed state is unchanged either way.
```

Skip when `self.meta.worktree` is `None` or empty. Otherwise, off-thread on
`spawn_blocking` exactly as today (the byte funnel must never block on SQLite),
call `db.put_session_attention(&SessionAttention { session, worktree_path, title,
body, since: thegn_core::util::now() })`. Keep the existing `tracing::warn!` on
error.

**The opt-in inbox row.** When `self.cfg.notifications.agent_attention_inbox` is
true (chunk 2's key; `self.cfg` is the `Arc<Config>` the actor already holds),
_also_ write the audit row — as **delete-then-insert per session**, not a bare
append, so the table holds one current row per session rather than one per turn:
delete unread rows with this `source_ref` (the session id, as today), then
`put_notification("agent_attention", &source, &message, &worktree)`. Comment that
this path deliberately bypasses `notify::record` because the daemon may be a
separate process with no `NotifyState`, and that it is off by default.

> Use the existing `NotificationStore` surface for the delete if one fits; if
> nothing does, prefer `mark_notification_read` over adding a trait method —
> keep this opt-in path cheap.

**`on_input` (line 773).** It already clears `self.attention`; when it did hold a
signal, also clear the row off-thread:

```rust
    fn on_input(&mut self) {
        self.activity.note_input(unix_now_secs());
        if self.attention.take().is_some() {
            self.publish_state();
            // The user answered — lower the hand in the shared table too, or
            // the worktree stays Blocked forever (the old notification row did
            // exactly that, against this capability's own spec).
            self.clear_attention_row();
        }
    }
```

**Session end.** Clear the row where the actor tears down (the same place the
tombstone is written / `SessionExit` is emitted) — a killed pane must not leave a
permanently-raised hand.

**Registry boot.** Call `clear_all_session_attention()` where the session map is
created empty: the daemon's startup (`crates/thegn-host/src/daemon/`), and the
host's in-process pane path when `[daemon] enabled = false`. No live sessions ⇒
no live hands. Grep for where `sessions: Arc<tokio::sync::Mutex<HashMap<…>>>` is
first constructed.

Factor the three clears into one small `fn clear_attention_row(&self)` helper on
`SessionActor` rather than repeating the `spawn_blocking` block.

**Existing test.** `an_osc_attention_signal_blocks_and_input_clears_it`
(`session.rs:1593`) tests the live feed state and should stay green untouched —
if it breaks, the live half regressed. Add a test asserting **no** notification
row is written for an OSC signal with the default config, and that a
`session_attention` row appears and then disappears after stdin.

### 3. `crates/thegn-host/src/attention_status.rs` — the consumer

Beside the merge-queue and roster reads (lines ~170-195), add one small table
read on the same off-loop worker:

```rust
    // Live raised hands (OSC 9 / OSC 777), one small table read like the two
    // above. These used to arrive as unread `agent_attention` notification rows;
    // they are state now, so they clear when the user answers (THE-68).
    let raised: std::collections::BTreeMap<String, i64> = db
        .list_session_attention()
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.worktree_path, a.since))
        .collect();
```

`list_session_attention` is ordered oldest-first, so collecting into a map keeps
the **oldest** hand per worktree only if you fold with "keep the smaller" — write
it explicitly (`.min()` on collision) rather than relying on iteration order, and
say why in a comment: two sessions in one worktree should report the longest
wait, matching the sort's tie-break.

Then in the `AttentionInputs { … }` literal (line ~254):

```rust
            attention_signal_since: raised.get(path).copied(),
```

### 4. `crates/thegn-host/src/actions.rs` — per-worktree ack (`x`)

In `ack_attention` (line 1152), beside the existing
`mark_notifications_read_for_worktree` call at line 1174:

```rust
                // Same item from the other side: a quieted needs-you worktree
                // must also lower its live raised hand, or the demand returns
                // on the very next hydration.
                let _ = db.clear_session_attention_for_worktree(&path);
```

### 5. `crates/thegn-host/src/handlers/attention.rs` — "clear all"

In `mark_all_read`, inside the `spawn_blocking` closure, after the
notification-clear `match` (chunk 1 edits the `(false, Some(wt))` arm just above;
this is a new statement after the whole `match`, so rebasing is mechanical):

- **scoped clear** (`(false, Some(wt))`): `clear_session_attention_for_worktree`
  for each path in the scoped set;
- **unscoped clear** (the `_` arm, the `g` all-worktrees view):
  `clear_all_session_attention()`.

Comment it: the live hands are the same demand the inbox rows are; clearing one
and not the other is exactly the class of bug THE-68 reported.

Do not change the optimistic model update or the status strings.

---

## Approach notes

- **Do not touch `attention.rs:484`.** The `NotificationKind::AgentAttention` arm
  stays; deliberate pushes still produce rows and still score.
- **Do not touch the render path.** The signal reaches the frame through
  `SidebarStatus`, which already marks chrome dirty. `render_plan` and its
  invariant tests must stay untouched and green.
- **0% idle:** no timer, no new wake source. The daemon write is `spawn_blocking`
  off the byte funnel; the host read is on the hydration worker beside
  `list_merge_queue`. If you catch yourself adding a poll, stop.
- Every new `let _ =` is a best-effort cache write — add `// best-effort:` so
  `test/ignored-result-ratchet.txt` (shrink-only) does not need a new line.
- If an e2e snapshot moves, the chrome changed and you should understand why
  before re-recording with `just e2e-update`. By design it should not.

## Done criteria

- [ ] `just quick thegn-core && just quick thegn-host` clean.
- [ ] `cargo nextest run -p thegn-core attention` passes, including the new
      raised-hand scoring tests.
- [ ] `cargo nextest run -p thegn-host attention` and
      `cargo nextest run -p thegn-host -- daemon::session` pass, including the
      pre-existing `an_osc_attention_signal_blocks_and_input_clears_it`.
- [ ] New test: an OSC signal writes **zero** notification rows under the default
      config, and writes exactly one `session_attention` row that disappears on
      stdin.
- [ ] With `agent_attention_inbox = true`: repeated signals from one session
      leave **one** row, not one per signal.
- [ ] Manual, in an isolated state dir (`just start name=the68`): 1. `printf '\033]9;Claude is waiting for your input\007'` in a pane →
      sidebar dot red / `✋` chip counts it, **inbox empty**; 2. type into the pane → the demand clears with no inbox interaction; 3. `thegn notify push --urgency alert …` → still one inbox row; 4. `x` on the needs-you row, and `a` in the inbox, both leave the worktree
      quiet across a rehydrate.
- [ ] `grep -n 'put_notification("agent_attention"' crates/thegn-host/src/daemon/session.rs`
      shows the write only inside the `agent_attention_inbox` branch.
- [ ] Before push: `THEGN_ALLOW_HEAVY=1 just test`,
      `THEGN_ALLOW_HEAVY=1 just coverage` (core ≥95%), `just smoke`.
