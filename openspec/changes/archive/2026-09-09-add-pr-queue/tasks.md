# Tasks — PR queue

## 1. Task kinds (thegn-core)

- [x] 1.1 Add `PrCiFailure` / `PrConflict` / `PrReview` to
      `agent_task::TaskKind` with their own `prompt_vars` and built-in prompts.
      The PR rules are the **inverse** of the merge queue's: the agent MUST push
      (that is how a PR advances) and MUST NOT merge the PR.
- [x] 1.2 Unit-test the new kinds' defaults render and validate.

## 2. Config (thegn-core)

- [x] 2.1 New `config_pr_queue.rs`: `PrQueueConfig` + `Default` + re-export,
      following the `config_ci.rs` sibling-module pattern.
- [x] 2.2 `config_enum!`s: `PrMergeMode` (`auto_merge`/`thegn`/`ready`),
      `PrMergeMethod` (`squash`/`merge`/`rebase`), `PrAutoEnqueue` (`off`/`worktree`).
- [x] 2.3 `PrQueueOverlay` + `[workspace.<slug>.pr_queue]`, exhaustively
      destructured like `MergeQueueOverlay`.
- [x] 2.4 `watch` list gating which blockers may wake the agent.
- [x] 2.5 `[pr_queue.prompts]` (ci_failure / conflict / review), wired into
      `config_validate` alongside the merge-queue templates.
- [x] 2.6 Document every key in `config/config.toml.example`.
- [x] 2.7 Extend the `config_enum!` round-trip test for the new enums.

## 3. Pure classification (thegn-core)

- [x] 3.1 New `pr_queue.rs`: `Blocker`, `PrqStatus`, `QueueAction`, `PrQueueFacts`.
- [x] 3.2 `classify(&PrStatus, cfg) -> Blocker` off fields already on `PrStatus`.
- [x] 3.3 `decide(blocker, facts, cfg) -> QueueAction` encoding every team-safety
      rule.
- [x] 3.4 `attempts_reset(prev_head, new_head, cfg)` — budget refills only on a
      head thegn did not create.
- [x] 3.5 Exhaustive table tests: draft never merges, approval gate, foreign push
      pauses, no-worktree ⇒ needs_human, own_prs_only, watch-list gating,
      merge_mode routing, attempt exhaustion.

## 4. Persistence (thegn-core)

- [x] 4.1 `pr_queue` table DDL + `user_version` bump + additive migration.
- [x] 4.2 `PrQueueRow` (Serialize, for `--json`).
- [x] 4.3 Store methods on `WorktreeAuxStore`: enqueue/update/remove/clear/list.
- [x] 4.4 CRUD + migration-ladder tests.

## 5. Forge seam (thegn-svc)

- [x] 5.1 New `prq.rs`: `PrQueueForge` trait (fetch, set_auto_merge, merge,
      threads, reply, rerun_failed).
- [x] 5.2 `GithubPrq` delegating to the existing `thegn_core::github` functions —
      no new GitHub code.

## 6. Driver + poller (thegn-host)

- [x] 6.1 `pr_driver.rs` — fetch → classify → decide → act, with the merge
      queue's `DriveStep`/`DriveOutcome` progress shape.
- [x] 6.2 Agent dispatch through `agent_task` + `agent_run`; `--force-with-lease`
      push guard.
- [x] 6.3 Polling: a `RefreshKind::PrQueue` ticker slot off
      `[pr_queue] poll_interval_secs` (emitted only while enabled), the
      remote-ref (push) kick alongside the existing `Pr`/`Ci` ones, offline
      gating in `connectivity_gate`, and per-row exponential fetch backoff in
      `pr_driver` reusing `ci_refresh::backoff_secs`. No separate
      `pr_queue_refresh.rs` module was needed — the driver already owns the
      per-row state a poller would have held.
- [x] 6.4 Wire into `hydrate.rs` ticker + `run.rs` drain.

## 7. CLI (thegn-host)

- [x] 7.1 `cmd/pr_queue.rs`: `add`/`list`/`rm`/`clear`/`status`/`drain`, `--json`,
      nested under the existing `pr` namespace.
- [x] 7.2 Refuse with guidance when disabled (non-zero exit), like `merge`.

## 8. UI (thegn-host)

- [x] 8.1 `Section::PrQueue` + `panel/sections/pr_queue.rs` + section keys.
- [x] 8.2 `handlers/pr_queue.rs` — pure `row_action_for(key, status)` matrix,
      off-loop spawners, channel drain, in-place row patch.
- [x] 8.3 Three `NotificationKind`s + statusbar badge + attention scoring.
- [x] 8.4 Three `ACTION_SPECS` entries with keywords, palette-gated on `enabled`.

## 9. Docs + validate

- [x] 9.1 New `docs/help/pr-queue.md` claiming all three action ids and the
      `panel:prq` context (the help + context ratchets enforce both).
- [x] 9.2 Add the PR-queue item to `tasks.md` group **Z**.
- [x] 9.3 `git add` new modules before nix-build (the flake source allowlist only
      sees git-tracked files).
- [x] 9.4 Run `just ci`.
