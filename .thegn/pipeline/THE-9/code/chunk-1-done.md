# THE-9 chunk 1 completion

Implemented the core merge-queue view policy and workspace-row projection.

- Added `thegn_core::merge_queue_view` with `MqTier`, `MqRollup`, `rollup`,
  `MqTokenFit`, and `fit_token`, including focused policy tests.
- Added `SidebarRow::mq_rollup`, populated from child worktree `mq_status`
  values by `workspace_slug` before visibility/filtering. Collapsed and dormant
  workspace rows remain covered; empty and landed-only queues stay silent.
- No rendering, input handling, statusbar, help, config, provider, or ratchet
  files were changed.

## Verification

- `just quick thegn-core` — passed
- `cargo nextest run -p thegn-core merge_queue_view` — 4 passed
- `just quick thegn-host` — passed
- `cargo nextest run -p thegn-host sidebar::tests` — 66 passed
- `cargo fmt --all -- --check` — passed after formatting
- `git diff --check` — passed

## Unverified

- E2E runs and muse snapshot updates were not run, per the chunk dev-loop
  policy. The existing snapshot baselines listed in the architect design remain
  for the deliberate e2e/update pass.
- Full-workspace gates (`just test`, `just ci`, coverage, and e2e) were not run.
