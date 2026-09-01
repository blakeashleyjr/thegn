# Chunk 2 — off-loop issue pickup, implement-to-PR bridge, and status projection

## Dependency and overlap

This chunk depends on Chunk 1's `AutopilotConfig`, pure policy, and
`AutopilotStore`; run it serially after Chunk 1. Its files are disjoint from
Chunk 1's files. Within the in-flight work, consume THE-27's review behavior
and THE-48's CI behavior after their APIs land; do not edit or recreate their
cache/autofix modules.

## Files touched

- `crates/thegn-host/src/autopilot_driver.rs` — new off-loop orchestration for
  issue pickup, worktree/session handoff, result validation, git push, forge PR
  creation, queue insertion, merge completion, and bounded diagnostics.
- `crates/thegn-host/src/main.rs` — register the new host driver module and the
  `autopilot` CLI command in the existing Clap command tree.
- `crates/thegn-host/src/hydrate_tracker.rs` — invoke the driver after a
  successful provider fetch/cache pass, preserving the existing ticker,
  off-loop boundary, and waker behavior.
- `crates/thegn-host/src/handlers/pr_queue.rs` — observe the existing off-loop
  `merged` transition and hand it to the driver; do not change PR policy or
  classify/merge logic.
- `crates/thegn-host/src/cmd/mod.rs` — register the command module.
- `crates/thegn-host/src/cmd/autopilot.rs` — implement read-only
  `thegn autopilot status [--json] [--repo PATH]` from the durable store, with
  disabled/default and bounded-output behavior.
- `crates/thegn-host/src/cmd/session.rs` — add the CLI capability projection to
  the existing `cli_control_caps` coverage list.
- `crates/thegn-core/src/control.rs` — add the stable `AutopilotStatus` read
  verb and required read scope, without adding a remote route.
- `crates/thegn-core/src/capability.rs` — add exactly one CLI-only
  `autopilot.status` catalog row and its coverage tests; do not add a
  `SURFACE_GAPS` excuse.
- `docs/help/cli.md` — document the status command, JSON shape, repo selection,
  and disabled behavior.
- `docs/help/workflows.md` — document issue → session → PR → queue → merge
  ownership, failure boundaries, and the fact that review/CI remain the
  existing PR queue loop.
- `test/completion-slot-ratchet.txt` — update the completion ratchet for the
  new command/value-taking repo argument using the existing generator/format.

## Approach

1. Add one host module and keep `run.rs`, `hydrate.rs`, and provider files out
   of scope. The existing issue refresh calls the driver only for the current
   repo's successful provider results. It supplies the authenticated
   `filter_assignee_me` provenance; the driver deduplicates provider/account/
   issue keys before attempting claims.
2. For each pure-match issue, call the Chunk 1 atomic claim before creating a
   worktree or making any tracker/forge write. Enforce the configured active
   and attempt limits. If the claim loses a race, return quietly. Immediately
   write the existing `agent_dispatches` roster row using `NewDispatch` with
   `stage = "autopilot"` and the resolved configured role, then attach its id
   to the run. This is the queued dispatch row existing supervisors already
   understand; do not invent a second role/dispatch store. Create/link the
   worktree through the THE-57 seam and record every step in the run row.
3. Mark the issue `InProgress` through `IssueRouter` after the claim and before
   the worker. This is an idempotent best-effort edge write. A provider error is
   recorded locally and must not release/retry the claim automatically.
4. Launch the configured arbitrary command using the existing
   `agent_run`/`TaskKind::Issue` path and its data-only prompt variables. Do not
   add a model/AI dependency, prompt family, scheduler, or new provider. The
   worker is instructed to commit but not push/open/merge; the host remains the
   only PR bridge.
5. Validate the worker result off-loop before any remote write: worktree is the
   claimed path, branch is the claimed branch, git status is clean, and the
   branch contains a commit ahead of the configured base. Reject empty/no-op,
   dirty, detached, conflicting, or unexpected-ref outcomes as `needs_human`.
   Preserve the worktree and diagnostics; do not silently create a second
   attempt.
6. Push through `GitBackend`/`BranchOps::push_set_upstream` to the configured
   remote. This new-branch path must not call force or force-with-lease. Then
   call the existing `Forge::create_pr`; title/body include issue identity and
   canonical URL, with issue body treated only as quoted data. Resolve and
   persist the PR number/head through the forge seam, not a vendor CLI parser.
7. If the repo-resolved `[pr_queue]` is enabled, insert the PR into the
   existing queue store with the claimed worktree/branch. The queue owns all
   subsequent review, CI, conflict, approval, and merge policy. If disabled,
   leave the run at `pr_opened` and make that visible in status.
8. In `handlers/pr_queue.rs`, process only the driver's existing transition
   callback while already off-loop. When the actual queue row status is
   `merged` (not merely “merge requested”), call the driver with repo + PR
   number. The driver finds the matching autopilot run, performs an expected-
   state transition, and—only when `done_on_merge` is true—sends `Done` through
   the existing `IssueRouter`. It must not use broad `move_on_merge` as an
   autopilot substitute or modify an unrelated linked issue.
9. Add the narrow CLI read surface. It reports repo, issue key, state, attempt,
   worktree, branch, PR number/url, timestamps, and bounded reason. It never
   starts/stops/retries work and never emits bodies, secrets, command output,
   or CI logs. Use the existing `emit_json` convention for `--json`.
10. Update the catalog, CLI capability projection, help, completion ratchet,
    and any generated control/catalog snapshot required by the existing tests
    in the same chunk. The row is CLI-only by design; do not add HTTP/gRPC/MCP/
    plugin handlers or a `SURFACE_GAPS` entry.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host autopilot`
- `cargo nextest run -p thegn-host hydrate_tracker`
- `cargo nextest run -p thegn-host pr_queue`
- `cargo nextest run -p thegn-core capability`
- `cargo nextest run -p thegn-host completion`

Run only the scoped crate/filter tests above. Do not run `just test`, `just ci`,
a full-workspace compile, e2e, or a live-state binary. If a CLI smoke check is
needed, set `XDG_STATE_HOME` to a fresh temporary directory first.

## Done criteria

- With autopilot disabled, issue refresh, manual dispatch, PR queue, and tracker
  status behavior are unchanged.
- With it enabled, a matching assigned/labeled issue claims at most once,
  creates one existing `agent_dispatches` roster row with the configured role,
  creates one linked worktree, runs the configured `TaskKind::Issue` handoff
  off-loop, and records every state/error transition.
- Dirty/no-commit/wrong-branch worker results never push or open a PR. A valid
  result pushes a new branch without force, opens one forge PR, and enqueues it
  only when the repo PR queue is enabled.
- Only an observed merged queue row for the recorded PR can close the run; the
  optional Done sync is idempotent and provider-seam based.
- THE-27 review handling and THE-48 CI logs/autofix are consumed, not
  duplicated; no new `TaskKind`, vendor API, pipeline scheduler, or CI cache is
  introduced.
- `autopilot.status` is cataloged as CLI-only, covered by the CLI capability and
  completion/help ratchets, bounded, and inert as a mutating surface.
- Commit exactly as:

  `feat(the-56): add off-loop issue pickup and PR bridge`
