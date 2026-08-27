# Architect review — THE-68

**Branch:** `tg/the-68-log-noise`
**Reviewed:** `main..HEAD` against `.thegn/pipeline/architect/design.md`
**Verdict: APPROVED** — no revision chunks. Two small corrections applied here
(`690103aa`), plus a clean rebase onto current `main`.

---

## Design conformance

Both halves of §3 landed as specified, and every §5 contract is exact.

**D1 — a raised hand is live state.** `session_attention` carries the DDL from
§5 verbatim; `SessionAttention` matches the published struct field for field;
`AttentionInputs::attention_signal_since` scores through the **existing**
`(Blocked, AgentNeedsInput)` arm, mirroring `stage_blocked_since`. No new tier,
reason, notification kind or surface — as required. The `[notifications]
agent_attention_inbox` knob defaults `false` and has a real
`THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX` overlay, so `env-overlay-ratchet.txt`
did not grow.

All seven lifecycle arms from §5 are implemented and I verified each: upsert on
`on_attention`; delete on `on_input`; delete on session end (`on_exit`);
`clear_all` on daemon boot **and** on host boot when the daemon route is off;
cascade on `del_worktree` (plus `del_worktrees_for_repo`, which the design did
not ask for and should have); clear on `mark_all_read` and on the per-worktree
ack (both the scoped-paths loop and the ack loop, which is what covers a hand
raised on the main checkout); and the 7-day sweep in `startup_prune`.

**D2 — "clear all" clears what the inbox shows.** `notification_scope::shows_in_repo_inbox`
is pure, documented and exhaustively tested; `hydrate_feed` and
`mark_notifications_read_scoped` both project it. The asymmetry cannot return
because there is no second copy.

**§4 invariants.** Schema 56 → 57 with a `ver < 57` one-time retirement
following the v46 `process_failed` precedent. No render-path edits, no new wake
source, no blocking I/O on the loop (the boot clear is an off-thread
`Background`-QoS worker). New core logic went into a new sibling module, not into
`db.rs`/`config.rs`. Every new `let _ =` carries a `// best-effort:` rationale
and no ratchet file grew. Help prose updated in `bars.md` / `panel.md` with no
`ACTION_SPECS` churn, as predicted.

**§7.** The `disk_cleaned` unknown-kind finding was correctly left out of scope.

## Gates run

| Gate | Result |
| --- | --- |
| `cargo nextest run -p thegn-core` | 3380 passed |
| `cargo nextest run -p thegn-host` | 2334 passed |
| `just quick thegn-host` (clippy) | clean |
| `just coverage` | core ≥95% lines |
| ratchets (platform / glyph / color / key / help) | pass |
| `just openspec-validate` | 166 passed, 0 failed |

## Corrections applied (`690103aa`)

1. **The opt-in audit row is now deleted, not marked read.** Design §3 and the
   shipped prose in `config.toml.example`, `CHANGELOG.md` and `docs/help/panel.md`
   all promise "one **current** row per session, not one per turn". The inbox
   lists read rows too (`get_all_notifications`), so marking the superseded row
   read still grew the list by one entry per agent turn — the exact pile THE-68
   is about, only greyed. `delete_notification` was already on the trait, so this
   is the "use the existing surface if one fits" branch chunk 3 offered. The
   documentation was correct; the code was not.
2. **Pinned it, and closed a wiring gap.** The daemon test now asserts *total*
   rows rather than only unread ones, so mark-read cannot silently come back. And
   `attention_status::collect_attention`'s new read + longest-wait fold had no
   end-to-end test at all — the scorer was covered in `thegn-core`, the store in
   `db_tests.rs`, but not the join. Added
   `a_raised_hand_row_blocks_the_worktree_and_folds_to_the_oldest`: a
   `session_attention` row with no notification row anywhere scores
   `Blocked`/`AgentNeedsInput`, two hands in one worktree report the longer wait,
   and lowering them clears the demand.

**Rebased onto `main` (`4fe3e6bc`), clean.** This was not optional: main's
`7ce6d634` fixes a `clippy::manual_ok_err` in `sandbox_cpucap.rs` that the branch
predated, so `just quick` failed before the rebase on debt that is not this
lane's. `SCHEMA_VERSION` is still 56 upstream, so the v57 bump — the conflict
point §4 called out — did not collide.

## Flagged, not blocking

Follow-ups, deliberately not folded in:

1. **`spawn_blocking` ordering between the raise and the lower.** `on_attention`
   queues the insert and `on_input` queues the delete as independent blocking
   tasks; the pool guarantees no ordering. A user answering inside that
   sub-millisecond window could have the delete land first, leaving a hand up
   until the next signal, session end, or the 7-day sweep. Very low probability
   and self-healing, but it is a new race the notification path did not have.
   Serializing the actor's DB writes onto one channel would close it.
2. **`mark_notifications_read_scoped` with an empty `all_known` marks every row
   read**, other repos' included — contradicting the trait doc's own promise.
   It is consistent with the display predicate and deliberately tested, but the
   empty set also arises from a *failed* `db.worktrees()` read, where the
   display's fail-open is harmless and the clear's is not. Worth distinguishing
   "registry is empty" from "registry read failed" at the call site.
3. **`SessionAttention::{title, body}` are written and never read.** Specified in
   §5, so conformant; note it if nothing consumes them by the next change.
4. **File the `disk_cleaned` follow-up** recommended in design §7.
