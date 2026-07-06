# Tasks — agent-driven merge-queue driver

## 1. Config

- [x] 1.1 Add `agent_command`, `auto_land`, `agent_max_attempts`,
      `agent_timeout_secs` to `MergeQueueConfig` + `Default` (docs condensed to hold
      the `config.rs` ratchet ceiling).
- [x] 1.2 Document the keys in `config/config.toml.example`.

## 2. Single-branch land primitive

- [x] 2.1 `integrate::attempt_land` → `AttemptOutcome`
      (`Landed`/`Ready`/`Conflict`/`GateFailed`/`UpToDate`), sharing fold/gate/CAS/
      resync with `run_fold`.
- [x] 2.2 `gate_tip` captures combined output (fed to the agent on a red gate).
- [x] 2.3 Sequenced throwaway-worktree path (`tmp_path`) — fixes a
      seconds-resolution collision between concurrent gate worktrees.

## 3. Serial driver

- [x] 3.1 `merge_driver::drive_queue` — per-branch fold→gate→land or agent
      dispatch→re-attempt, with `needs_human` after `agent_max_attempts`.
- [x] 3.2 Headless `run_agent` (sh-quoted template, login shell, cwd = worktree,
      git-env scrub, timeout watchdog).
- [x] 3.3 Pure, unit-tested prompt composition.

## 4. CLI

- [x] 4.1 `merge` namespace: `add [--all]`/`list`/`rm`/`clear`/`drain [--all]`/
      `land`, `--json`.
- [x] 4.2 `MergeQueueRow: Serialize`; add `merge` to the grouped-help table.

## 5. UI

- [x] 5.1 Panel `merge_queue` glyphs + reasons for `agent_running`/`ready`/
      `needs_human`.

## 6. Tests

- [x] 6.1 `attempt_land` outcomes over a real-git fixture.
- [x] 6.2 `drive_queue` end-to-end with a fake agent (resolves → lands; fails →
      `needs_human`).
- [x] 6.3 `smoke.sh` `merge add`/`list`/`drain`/`rm`.

## 7. Follow-ups (deferred)

- [ ] 7.1 In-TUI `Action::QueueMerge`/`DrainQueue` + `auto_drain`-on-`AgentEnd`
      (needs a `run.rs`/`keymap.rs` extraction to clear ratchet headroom).
- [ ] 7.2 Interactive assign/drain from the panel section.
