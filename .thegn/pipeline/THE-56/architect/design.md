# THE-56 — PR workflow audit and automation design

Status: proposed architecture; implementation is split into the two serial
coder chunks in `../code/`.

## Decision

Thegn already contains most of the individual PR-loop primitives. The missing
glue is a small, opt-in supervisor path that turns a provider-filtered issue
refresh into one durable issue claim and existing `agent_dispatches` roster
row, runs the existing `TaskKind::Issue` handoff, and—only after a locally
verified result—pushes a branch, opens a PR, and places it in the existing PR
queue. A second off-loop hook closes the correlated autopilot run and optionally
marks the issue done when the PR queue observes a real merged transition.

This design deliberately does not add a workflow engine, an AI provider, a new
agent task kind, a vendor-specific tracker/forge API, or a second PR/review
queue. The worker remains the configured arbitrary command launched through
the existing `agent_task`/`agent_run` path. The core owns pure matching,
state-transition, and claim invariants; the host owns network, git, process,
and database edges.

The implementation is disabled by default and enabled per trusted workspace
overlay (`[workspace.<slug>.autopilot]`). A repository-root `.thegn.*` file
cannot enable it. When disabled, the issue refresh and PR queue retain exactly
their current behavior.

## Inputs audited

The openspec change `openspec/changes/add-issue-autopilot/` was read in full:
`proposal.md`, `design.md`, `tasks.md`, `specs/autopilot/spec.md`, and
`specs/state-db/spec.md`. It is a draft, not an authority. The current branch
re-check found the following stale or over-broad claims:

- A new `TaskKind::IssueImplement` is unnecessary. `TaskKind::Issue` already
  exists and has a data-only issue prompt, inverse push/merge rules, stable
  prompt variables, and unit tests (`crates/thegn-core/src/agent_task.rs:30-45`,
  `:401-412`, `:451-534`, `:1415-1525`).
- Issue-to-worktree creation is already implemented by THE-57:
  `wt new --from-issue` resolves the configured tracker and links the issue
  (`crates/thegn-host/src/cmd/wt.rs:95-229`, `:472-485`), and the catalog already
  projects `worktrees.create` (`crates/thegn-core/src/capability.rs:628-638`).
- The issue panel already has a manual `D` dispatch using the existing issue
  task handoff (`crates/thegn-host/src/handlers/tracker.rs:151-358`). The gap is
  automatic, deduplicated pickup—not another manual dispatch mechanism.
- Issue hydration is already off-loop and periodic
  (`crates/thegn-host/src/hydrate_tracker.rs:13-124`,
  `crates/thegn-host/src/hydrate.rs:620-700`). The implementation should hook
  that producer, not add a polling thread or wake-less loop.
- PR queue driving, merge policy, inverse prompt families, and queue rows
  already exist (`crates/thegn-core/src/pr_queue.rs`,
  `crates/thegn-host/src/pr_driver.rs:132-275`, `:553-625`).
- CI rerun already exists at the provider seam and in the CLI/TUI
  (`crates/thegn-svc/src/ci.rs`, `crates/thegn-host/src/cmd/ci.rs:1-107`,
  `:220-300`, `crates/thegn-host/src/actions.rs:227-350`). The missing
  catalog projection and bounded cached logs belong with THE-48's CI work;
  this change must not duplicate them.
- PR merge-to-issue status sync already exists behind
  `[issues].move_on_merge`, but it is broad linked-issue behavior and only
  handles merge, not an autopilot-owned open/working lifecycle
  (`crates/thegn-host/src/hydrate.rs:3482-3595`,
  `crates/thegn-core/src/config_issues.rs:15-38`).
- The draft's schema version 62 cannot be copied here. THE-27 and THE-48 each
  independently reserve/add schema 62 in their designs. THE-56 must rebase
  after those changes and use the next reconciled version (expected v63 when
  both land), with one additive migration that verifies all three tables.

## Current gap matrix

The statuses below describe this branch, not the future state of the in-flight
THE-27/THE-48 branches. “Done” means the primitive is present and reusable;
“partial” means a manual or narrower path exists; “missing” means there is no
safe autonomous bridge.

| Pattern step                                                 | Thegn surface and evidence                                                                                                                                                                                                                                     | Status                       | THE-56 disposition                                                                                                                                                           |
| ------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Assigned/consented issue becomes eligible work               | Provider issue model has assignees, labels, status, and filters (`crates/thegn-core/src/issue.rs:11-163`); refresh uses provider-side `filter_assignee_me` (`crates/thegn-host/src/hydrate_tracker.rs:49-64`), but only caches/issues-notifies.                | Partial                      | Add exact trigger-label + Todo matching and use the already authenticated “my issues” filter as the v1 consent boundary.                                                     |
| Assignment/label event is noticed                            | Periodic issue refresh is off-loop and pulses the waker (`crates/thegn-host/src/hydrate_tracker.rs:61-123`); no watcher/claim path follows a successful fetch.                                                                                                 | Missing                      | Chunk 2 hooks pickup into the refresh producer; no new timer.                                                                                                                |
| Duplicate claims are prevented                               | `agent_dispatches` records manual dispatches and `issue_links` binds worktrees, but neither is an atomic autopilot claim ledger (`crates/thegn-core/src/store/worktree_aux.rs:156-163`; `crates/thegn-core/src/db_notification.rs:298-321`).                   | Missing                      | Chunk 1 adds a unique claim/correlation row; Chunk 2 also writes the existing queued `agent_dispatches` row with the configured role.                                        |
| Issue creates a session/worktree                             | `wt new --from-issue` and manual `D` dispatch already create/link worktrees (`crates/thegn-host/src/cmd/wt.rs:170-229`, `crates/thegn-host/src/handlers/tracker.rs:224-358`).                                                                                  | Partial                      | Reuse those seams from one off-loop driver; no parallel worktree implementation.                                                                                             |
| Data-driven agent handoff runs                               | `TaskKind::Issue`, `default_prompt`, configured command selection, and session staging already exist (`crates/thegn-core/src/agent_task.rs:401-534`; `crates/thegn-host/src/cmd/session.rs:414-450`).                                                          | Done                         | Reuse unchanged. The issue body is data in the template, never executable instructions.                                                                                      |
| Worker validates and commits                                 | The issue prompt requires worktree-only edits, a clean status, and a commit (`crates/thegn-core/src/agent_task.rs:509-534`), but no autopilot supervisor checks the result before release.                                                                     | Partial                      | Chunk 2 performs host-edge status/commit/ahead checks; failed runs remain inspectable and are not pushed.                                                                    |
| Branch is pushed                                             | `GitBackend`/`BranchOps` expose push through the git seam (`crates/thegn-svc/src/git/mod.rs:237-335`, `crates/thegn-svc/src/git/branch.rs:1-84`), but the issue path does not compose it with a completed task.                                                | Partial                      | Supervisor pushes a new branch with ordinary upstream push only; never force-pushes.                                                                                         |
| PR is opened                                                 | Forge `create_pr` and the manual PR action are present and off-loop (`crates/thegn-core/src/forge/mod.rs:105-135`, `crates/thegn-host/src/actions.rs:646-702`).                                                                                                | Partial                      | Supervisor uses `Forge::create_pr` with issue URL/body provenance and records the returned PR through the existing forge seam.                                               |
| New PR enters PR/review loop                                 | `[pr_queue]`, pure classification, off-loop drive, and merge policy exist (`crates/thegn-core/src/config_pr_queue.rs:62-180`, `crates/thegn-host/src/handlers/pr_queue.rs:103-154`). `auto_enqueue` is configured but not currently consumed by host behavior. | Partial                      | Enqueue the created PR only when the repo PR queue is enabled; otherwise leave run at `pr_opened` with a clear local status.                                                 |
| Review comments become follow-up agent work                  | Existing PR queue has `Review` prompt family and inverse no-merge rule (`crates/thegn-host/src/pr_driver.rs:553-625`; `crates/thegn-core/src/agent_task.rs:451-507`). THE-27 adds diff anchoring.                                                              | Partial/done after in-flight | Reuse `TaskKind::PrReview`; do not duplicate THE-27's review cache or presentation.                                                                                          |
| CI failure is observed, logs are bounded, rerun is available | CI provider seam and manual rerun/log paths exist (`crates/thegn-svc/src/ci.rs`, `crates/thegn-host/src/cmd/ci.rs:220-300`). THE-48 owns cached/redacted logs and autofix.                                                                                     | Partial/done after in-flight | Do not add a second CI cache or `TaskKind`; THE-48 owns the cataloged CI action and `PrCiFailure` enrichment.                                                                |
| PR merge is observed                                         | PR hydration emits state transitions and can apply broad `move_on_merge` (`crates/thegn-host/src/hydrate.rs:3482-3595`).                                                                                                                                       | Partial                      | Chunk 2 listens to the PR queue's actual `merged` row transition and closes only the matching autopilot run.                                                                 |
| Tracker status syncs back                                    | `IssueRouter::update_issue` and `IssuePatch` support status writes (`crates/thegn-svc/src/issue/mod.rs:60-165`, `:380-410`); current automatic behavior is merge-only and not run-owned.                                                                       | Partial                      | Set InProgress after successful claim and Done only for the recorded run on real merge, guarded by `done_on_merge`; never overwrite unrelated issue changes.                 |
| Retry/recovery/stop                                          | PR/merge queues have bounded attempts and `agent_dispatches` has durable status/report/note data (`crates/thegn-core/src/config_pr_queue.rs:120-180`, `crates/thegn-core/src/db.rs:131-136`).                                                                  | Partial                      | Chunk 1 records terminal failure/needs-human and attempt count. Stop/retry/kill control is a follow-up because there is no durable process handle or safe cancellation seam. |
| Operator visibility                                          | Existing notifications and queue panels show tracker/PR events, but no autopilot row/CLI exists; the catalog is centralized (`crates/thegn-core/src/capability.rs:184-220`, `:731-760`).                                                                       | Partial                      | Chunk 2 adds a narrow CLI-only `autopilot status` projection and documents that it is read-only.                                                                             |

## Proposed flow

The flow is one bounded off-loop transaction chain, not an event-loop task:

1. The existing issue refresh fetches the provider's authenticated “my issues”
   result. If the repo's trusted `[workspace.<slug>.autopilot]` is disabled,
   it stops at today's cache behavior.
2. For enabled repos, a pure matcher accepts an issue only when its label is an
   exact configured label, its status is the configured pickup status (default
   `todo`), and the provider returned it through `filter_assignee_me`. Label
   matching is case-sensitive and whitespace-preserving after the provider's
   normal label normalization; no body text is interpreted.
3. The core store atomically inserts an `autopilot_runs` claim keyed by the
   stable provider-qualified issue id. A unique conflict means another refresh
   or host already owns it. `max_concurrent` is checked against non-terminal
   rows before claiming. Claims are never inferred from a stale cache.
4. After winning the claim, the host writes the existing `agent_dispatches`
   roster row with `stage = "autopilot"`, the resolved configured agent role,
   and `status = queued`, then links that dispatch id from the run row. This is
   the queued dispatch visible to existing supervisors; `autopilot_runs` adds
   only the claim/PR correlation that the roster does not have. The driver then
   creates/reuses the THE-57 issue worktree seam, links the issue, and
   best-effort writes `InProgress`. A status-write failure is recorded locally
   and does not make a duplicate claim possible.
5. The host launches the existing configured arbitrary command with
   `TaskKind::Issue` and the existing data-only prompt variables. The worker may
   edit and commit; the worker is not asked to open a PR or merge anything.
6. After the process exits, the driver validates: expected worktree, clean
   status, current branch, at least one commit ahead of the configured base,
   and no branch/ref movement that would require force push. It records a
   terminal `needs_human` result on any failure and does not push or alter the
   tracker beyond the initial InProgress attempt.
7. On success, the supervisor uses `GitBackend`/`BranchOps` to push the new
   branch with upstream tracking, then `Forge::create_pr` to open a PR. PR
   title/body must include issue number/title and the canonical issue URL; the
   issue body remains quoted data. The created PR number/head is persisted in
   the run row.
8. If `[pr_queue]` is enabled for the same repo, the driver enqueues the PR
   through the existing queue store. The PR queue then owns review, CI,
   conflict, and merge decisions. If disabled, the run is `pr_opened` and the
   operator can use existing PR actions.
9. The PR queue sends its existing off-loop transition stream. Only a row that
   transitions to `merged` and matches an autopilot run's repo + PR number may
   close that run. With `done_on_merge = true`, the host sends an idempotent
   `IssuePatch { status: Done, .. }` through `IssueRouter`; it must not use the
   broad linked-issue sweep as a substitute.

All network, git, process, forge, and SQLite work is off the compositor loop.
The producer pulses the existing waker after cache/ledger changes. No new
blocking poll, sleep, or background thread is permitted.

## Core contract and configuration

Chunk 1 adds `crates/thegn-core/src/autopilot.rs` with pure values and policy:

- `AutopilotState`: `Claimed`, `Working`, `PrOpened`, `Shepherding`,
  `NeedsHuman`, `Done`, `Stopped`.
- `AutopilotIssueKey`: provider + account + stable issue id, so two configured
  tracker accounts cannot collide accidentally.
- `matches_issue` and `can_claim` are pure and exhaustively unit-tested.
- `transition` rejects backward/terminal transitions and records a bounded
  reason, attempt, and optional PR number. It has no DB, clock, process, git,
  tracker, or forge dependency.

The trusted workspace overlay is intentionally small:

```toml
[autopilot]
enabled = false
trigger_label = "agent-ready"
assignee = "me"
pickup_status = "todo"
max_concurrent = 1
max_attempts = 1
agent = ""
agent_command = ""
agent_timeout_secs = 1800
open_as = "ready"
done_on_merge = false
```

`assignee = "me"` means the provider-side authenticated `IssueFilter` already
used by issue hydration; v1 does not pretend to know a cross-provider identity.
Validation rejects other values until the tracker seam grows an explicit
identity capability. `agent` and `agent_command` follow the existing generic
agent picker/command template; no model, vendor, or AI SDK is introduced.
`open_as` is `ready` or `draft`. All keys must be represented in
`config/config.toml.example`, the configuration help/config-reference, and the
env-overlay ratchet. The workspace overlay is trusted user configuration; the
repo-root overlay may not enable the supervisor or widen its command/sandbox.

The existing global config value is the default; `Config::repo_autopilot` is
the only host lookup for a repo-scoped operation, mirroring
`Config::repo_pr_queue` (`crates/thegn-core/src/config.rs:6650-6661`).

## State schema and migration ordering

Add a separate core DB module and store trait rather than growing `db.rs` into
another god-file. The additive `autopilot_runs` table needs:

- provider/account/issue identity and repo root;
- the existing `agent_dispatches.id` correlation once the queued roster row is
  written;
- worktree, branch, base branch;
- state, attempt, optional PR number/head/url;
- created/updated/claimed/finished timestamps;
- bounded last error/reason.

The unique claim index is on the provider-qualified issue key. Reads are
bounded by repo and terminal state; status output must never dump issue bodies,
tokens, command expansions, or CI logs. The existing `agent_dispatches` row
remains the worker/role/status roster; do not add a second dispatch-status
vocabulary or replace that roster with the new table.

Current branch schema is v61 (`crates/thegn-core/src/db.rs:131-136`). Because
THE-27 and THE-48 both design v62 changes, Chunk 1 is explicitly serial after
those branches are reconciled. The coder must rebase and choose the next
available version—expected v63 when both changes land—and update the single
ladder in `db.rs`/`db_migrate.rs`. Do not add a competing v62 migration. The
migration must be additive, idempotent, verify its table/index, and retain
pre-existing rows. Unit tests cover fresh creation, upgrade, duplicate claim,
and reopen/readback.

## Catalog and surface policy

The status command is the only new external projection in this change:
`autopilot.status`, CLI-only, read scope. It reports disabled/enabled state and
bounded run summaries for the selected repo. It does not start work, stop a
process, retry a failed run, expose provider credentials, or become an HTTP,
gRPC, MCP, or plugin capability. The catalog row and `cli_control_caps`
projection must agree; no `SURFACE_GAPS` excuse is used for the CLI row.

This narrow projection is intentional: a control API for a process supervisor
requires a cancellable process/session handle and authenticated remote repo
scope. Those do not exist today. The command remains useful for auditability
without inventing either.

## Ratchets and documentation required in the same chunks

The coders must update ratchets in the same commit that changes the associated
surface:

- env-overlay coverage for every new config key, including the workspace form;
- completion-slot coverage for the new `autopilot status` command/arguments;
- control/catalog snapshots if the catalog schema or CLI capability projection
  changes;
- help page/config-reference coverage for the configuration and CLI command.

The status command should use the existing JSON convention from
`crates/thegn-host/src/cmd/mod.rs:67-75`, and help should explain the complete
flow, disabled default, provider-side “me” semantics, failure behavior, and
how to inspect the run without implying that thegn itself is an AI agent.

## In-flight boundaries

- **THE-27 (`tg/the-27-pr-comments-in-diff`)** owns review-comment cache,
  anchoring, diff presentation, and its schema migration. This design only
  consumes the existing `TaskKind::PrReview`/PR queue handoff.
- **THE-48 (`tg/the-48-ci-logs`)** owns CI log cache, redaction, CI detail
  routes/catalog, and autofix context. This design does not add CI log storage,
  another rerun action, or another CI task family. The requested “re-run CI”
  audit item is currently manual/done at the seam and should be cataloged by
  THE-48 with its CI changes.
- The openspec sibling `add-watched-pr-comment-tasks` is THE-22 and is out of
  scope.

## Follow-ups to file

1. **Tracker identity and claim events:** extend the provider seam with an
   explicit authenticated identity/capability and webhook/event cursor, then
   support `assignee = "any"` and assignment-only pickup without relying on
   provider-filter semantics.
2. **Safe stop/retry:** add a durable session/process handle and a cancellation
   seam before exposing `autopilot stop` or `autopilot retry` through control API,
   gRPC, MCP, or plugins. A state flip alone cannot stop a running command.
3. **Autopilot operator panel:** show run/worktree/PR/error rows in the native
   panel with notifications and explicit action ratchets after the read-only
   CLI proves the state model.
4. **Cataloged CI rerun:** THE-48 should project the existing provider-seam
   rerun operation as one `ci.rerun` catalog row and update its control/help/
   completion ratchets; do not add a duplicate row in THE-56.
5. **Webhook/push-triggered refresh:** add a provider-neutral event cursor or
   forge webhook ingress only after the periodic refresh path has production
   evidence. Polling is sufficient for this cheap, opt-in first bridge.
6. **Autopilot retry policy:** once cancellation and human review semantics are
   defined, add bounded retry with fresh worktree policy; the first version
   uses `max_attempts = 1` and preserves evidence.

## Implementation order and verification

Chunk 1 lands first and is rebased after THE-27/THE-48 migration reconciliation.
Chunk 2 is then applied serially because it consumes Chunk 1's config, state,
and store APIs. The chunks are file-disjoint as authored; the Lead must not
parallelize them due to this API dependency. Neither coder may run a live-state
`thegn` invocation; if a command is unavoidable, set `XDG_STATE_HOME` to a
fresh temporary directory.

Verification is scoped to the crate filters listed in each chunk. Do not run
`just test`, `just ci`, a full-workspace compile, or e2e.
