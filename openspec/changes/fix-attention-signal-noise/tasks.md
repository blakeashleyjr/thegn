# Tasks — an OSC raised hand is live state; "clear all" clears what it shows

Four chunks, land order 1 → 2 → 3 → 4. Chunks 1, 2 and 4 are mutually
independent; chunk 3 compiles only once chunk 2 is in, and rebases onto chunk 1
(both edit `handlers/attention.rs::mark_all_read`, adjacently).

## 1. "Clear all" clears exactly what the inbox shows (chunk 1)

- [ ] 1.1 New pure `crates/thegn-core/src/notification_scope.rs` with
      `shows_in_repo_inbox(worktree_path, repo_paths, all_known)`; register it in
      `lib.rs`. Header comment states why it is alone: the display filter and the
      clear carried separate copies and drifted.
- [ ] 1.2 Unit tests, one per arm plus the regressions: host-global always in
      scope; a `repo_paths` hit in scope; a known path of another repo out of
      scope; an unknown path in scope
      (`the_repo_main_checkout_has_no_registry_row_so_it_shows`); a path in
      `repo_paths` but absent from `all_known` still shows (the arms are an OR,
      not a precedence chain).
- [ ] 1.3 `mark_notifications_read_scoped` takes `all_known: &[String]` too
      (`store/notification.rs` trait + doc comment); `db_notification.rs`
      replaces the loop of UPDATEs with one statement carrying all three arms,
      building placeholders in the `unread_counts_for_kinds` style. Empty
      `repo_paths` drops that arm (SQLite rejects `IN ()`); empty `all_known`
      drops the `NOT IN` arm and marks everything — the correct fail-open answer,
      with a comment saying so.
- [ ] 1.4 `db_tests.rs`: `scoped_clear_marks_untagged_and_repo_rows` (updated for
      the new argument), `scoped_clear_marks_rows_the_registry_does_not_know`
      (**the regression**), `scoped_clear_with_empty_registry_marks_everything`,
      `scoped_clear_with_no_repo_paths_still_marks_untagged_and_unknown`.
- [ ] 1.5 Both call sites project the predicate:
      `hydrate_feed::populate_notifications` retains through it (local comment
      trimmed to a pointer), and `handlers::attention::mark_all_read` passes the
      registry set from the same `db.worktrees()` read the display uses.
      `grep -rn "worktree_path.is_empty() ||" crates/` returns exactly one site.

## 2. Core: the `session_attention` table + the config knob (chunk 2)

- [ ] 2.1 `osc_attention.rs`: the `SessionAttention` row type (session,
      worktree_path, title, body, `since` in unix seconds).
- [ ] 2.2 `db.rs`: `SCHEMA_VERSION` 56 → 57; the v57 DDL + index; the `ver < 57`
      one-time cleanup marking the unread `agent_attention` backlog read (v46
      `process_failed` precedent); the 7-day `prune_session_attention` sweep in
      `startup_prune` with its doc-comment bullet. DDL + version + the two
      `ver`-gated blocks only — no new logic in this god-file.
- [ ] 2.3 `store/notification.rs`: the six trait methods
      (`put_session_attention`, `clear_session_attention`,
      `clear_session_attention_for_worktree`, `clear_all_session_attention`,
      `list_session_attention`, `prune_session_attention`), each documented;
      `db_notification.rs` implements them (upsert `ON CONFLICT(session)`, list
      ordered `since ASC`).
- [ ] 2.4 `db_workspace.rs`: cascade the delete from `del_worktree` and
      `del_worktrees_for_repo`, beside the `attention_acks` one — a worktree
      recreated at the same path must not inherit an instant `Blocked` dot.
- [ ] 2.5 `NotificationsConfig.agent_attention_inbox: bool` (default `false`)
      with its doc comment; a **real** `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX`
      override in `Config::env_overlay` plus the overlay struct field and `apply`
      (`test/env-overlay-ratchet.txt` is shrink-only — pinning is not an option).
      Not added to `NotificationsOverlay`.
- [ ] 2.6 `config/config.toml.example`: document the key inside `[notifications]`
      (`tests/config_example.rs` gates it, and the runtime config-reference help
      page is generated from this file).
- [ ] 2.7 Unit tests — core is 95%-line gated and `db*.rs` is **not** in the
      justfile `cov_ignore` list: upsert replaces rather than appends;
      `list_session_attention` returns oldest-first; per-session, per-worktree and
      full clears; `prune_session_attention` returns the count and drops only past
      the cutoff; the `del_worktree` cascade; the migration ladder still reaches
      57; the `ver < 57` cleanup retires an unread backlog exactly once;
      `agent_attention_inbox` defaults off and the env knob flips it
      (`tests/env_overlay_coverage.rs` requires it be exercised, not merely
      declared).
- [ ] 2.8 Confirm nothing observable changed: storage + config only, the daemon
      still writes the old row.

## 3. Wire the live signal (chunk 3)

- [ ] 3.1 `attention.rs`: the `attention_signal_since` input and one `consider`
      arm mapping it to the existing `(Blocked, AgentNeedsInput)`, modelled on
      `stage_blocked_since`. Do **not** touch the `NotificationKind::AgentAttention`
      arm.
- [ ] 3.2 `attention.rs` tests, modelled on
      `stage_waiting_human_scores_as_blocked_through_the_existing_reason`: a hand
      alone scores `(Blocked, AgentNeedsInput)` with the given `since` and
      `needs_user()`; it invents no reason (assert equality with the notification
      path's); `None` scores nothing; two blocked worktrees sort longest-waiting
      first.
- [ ] 3.3 `daemon/session.rs`: `on_attention` upserts a `session_attention` row
      instead of appending a notification (skipping a session with no worktree),
      on `spawn_blocking` off the byte funnel; `on_input` lowers the hand when it
      held one; the actor teardown lowers it; the empty session registry
      (daemon boot, and the in-process path with `[daemon] enabled = false`)
      calls `clear_all_session_attention`. One `clear_attention_row` helper, not
      three copies of the `spawn_blocking` block.
- [ ] 3.4 `daemon/session.rs`: the `agent_attention_inbox` opt-in writes the audit
      row as delete-then-insert per session (one **current** row, never one per
      turn), with a comment on why it bypasses `notify::record`.
- [ ] 3.5 `attention_status.rs`: read `list_session_attention` on the hydration
      worker beside `list_merge_queue`, folding collisions with `.min()`
      explicitly (two sessions in one worktree report the longest wait, matching
      the sort's tie-break); feed `attention_signal_since` into the
      `AttentionInputs` literal.
- [ ] 3.6 `actions.rs::ack_attention` (`x`) also calls
      `clear_session_attention_for_worktree`, and
      `handlers/attention.rs::mark_all_read` lowers hands for the scoped set
      (`(false, Some(wt))`) or all of them (the `g` all-worktrees arm) — otherwise
      the live state becomes a new un-clearable nag.
- [ ] 3.7 Host tests: an OSC signal writes **zero** notification rows under the
      default config and exactly one `session_attention` row that disappears on
      stdin; with `agent_attention_inbox = true` repeated signals from one session
      leave one row; a signal from a session with no worktree writes no row; a
      deliberate `agent_attention` push still records a row; the ack and both
      clear-all arms lower the hands. `an_osc_attention_signal_blocks_and_input_clears_it`
      stays green untouched — if it breaks, the live half regressed.

## 4. Specs, help prose, changelog (chunk 4)

- [x] 4.1 This change folder: `proposal.md`, `design.md`, `tasks.md`, and delta
      specs for `activity-signals` and `notifications`.
- [x] 4.2 `docs/help/bars.md` + `docs/help/panel.md`: what `a` covers (this
      repo's rows plus host-global ones, including a row tagged to the main
      checkout — the fix; `g` widens to every worktree) and what a raised hand is
      (an agent's `OSC 9`/`OSC 777` "I need you", shown as the sidebar dot and the
      `✋` chip, cleared when you answer, not an inbox entry unless
      `[notifications] agent_attention_inbox` is on). Prose only — no new action
      id, so no help-ratchet churn.
- [x] 4.3 `CHANGELOG.md`: two **Fixed** entries under `[Unreleased]` — the inbox
      no longer fills with one row per agent turn (with the one-time migration and
      the opt-in), and "clear all" now clears every row the inbox shows.

## 5. Scenario → test mapping

Every scenario in the two delta specs maps to a test in chunk 1 or chunk 3.
Names in `code` are fixed by the chunk specs; the rest are the names the tasks
above prescribe.

| Delta spec / scenario                                                                    | Chunk | Test                                                                                                                                      |
| ---------------------------------------------------------------------------------------- | ----- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| activity-signals — A raised hand marks the worktree needs-you and leaves the inbox empty | 3     | `an_osc_signal_writes_no_notification_row` (3.7) + `a_raised_hand_scores_blocked_through_the_existing_reason` (3.2)                       |
| activity-signals — Answering the agent lowers the hand                                   | 3     | `a_raised_hand_is_lowered_on_stdin` (3.7), alongside the untouched `an_osc_attention_signal_blocks_and_input_clears_it`                   |
| activity-signals — A deliberate push still records an inbox row                          | 3     | `a_deliberate_agent_attention_push_still_records_a_row` (3.7)                                                                             |
| activity-signals — The opt-in holds one current row per session                          | 3     | `agent_attention_inbox_opt_in_keeps_one_current_row_per_session` (3.7)                                                                    |
| activity-signals — A signal with no worktree records nothing                             | 3     | `an_unattributed_signal_writes_no_row` (3.7)                                                                                              |
| notifications — A row tagged to the main checkout is shown and cleared                   | 1     | `the_repo_main_checkout_has_no_registry_row_so_it_shows` (1.2) + `scoped_clear_marks_rows_the_registry_does_not_know` (1.4)               |
| notifications — Another repo's known worktree is neither shown nor cleared               | 1     | `a_known_path_of_another_repo_is_out_of_scope` (1.2) + the second assertion of `scoped_clear_marks_rows_the_registry_does_not_know` (1.4) |
| notifications — An untagged row is shown and cleared                                     | 1     | `host_global_rows_are_always_in_scope` (1.2) + `scoped_clear_marks_untagged_and_repo_rows` (1.4)                                          |
| notifications — The all-worktrees view clears everything                                 | 3     | `clear_all_in_the_all_worktrees_view_lowers_every_hand` (3.6/3.7), over the existing unscoped `mark_all_notifications_read`               |
| notifications — Clearing lowers the live hands for the same scope                        | 3     | `ack_attention_lowers_the_worktrees_raised_hand` and `mark_all_read_lowers_hands_for_the_scoped_set` (3.7)                                |

## 6. Validation

- [ ] 6.1 Confirm no e2e baseline moved — chrome is unchanged by design (the dot,
      chip and ring fire on the same evidence). A moved snapshot means the chrome
      changed; understand why before re-recording with `just e2e-update`.
- [ ] 6.2 Rebase on `main` and re-check `SCHEMA_VERSION` (57 is a known
      cross-branch collision point).
- [ ] 6.3 Run `just ci` once (includes `openspec validate --all --strict` and the
      core coverage gate) — a pre-PR gate, not a per-edit command.
