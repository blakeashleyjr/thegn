# Issue autopilot — assign an issue, get a shepherded PR

Linear: THE-56

## Why

Every step of "assign issue → agent works → PR opens → PR gets babysat →
tracker syncs" already exists in thegn as a separate, manually-triggered
piece:

- **Issue → worktree → agent** is the `D`/`s` key in the work panel
  (`handlers/tracker.rs::dispatch_agent`; `add-issue-driven-worktrees`
  generalizes it) — one keypress _per issue, by a human at the keyboard_.
- **PR babysitting** is the PR queue (`add-pr-queue`, implemented): poll,
  classify, fix CI/conflicts/review feedback with a headless agent, let the
  forge merge. It starts at "a PR exists and someone queued it".
- **PR creation from a worktree** ships today (`forge::create_pr`, Z 335).
- **Status write-back** exists (`IssueRouter::update_issue` / `IssuePatch`),
  and `add-generic-tracker-model` is building first-class transitions.

Nothing composes them. The composed loop is the headline capability of the
comparison field this issue catalogs (37 tools for PR automation, 27 for
tracker→session: Devin, Copilot Workspace, Cursor, Conductor, vibe-kanban,
…), and thegn's worktree-per-tab model plus its two queues make it a thinner
delta here than in any of those tools: the missing piece is a _pickup loop
and a bridge_, not new machinery.

## What Changes

A new **autopilot** capability, **off by default** (`[autopilot] enabled =
false`), that closes the loop:

- **Pickup.** When the existing issue-tracker cache refresh completes
  (`RefreshKind::Issues` — no new wake source), issues matching the trigger —
  carrying the configured `trigger_label` AND matching the `assignee` policy
  (default `me`) AND in a configured pickup status (default todo) — are
  claimed, bounded by `max_concurrent` (default 1). A claim is durably
  recorded (new `autopilot_runs` table) so re-polls and restarts never
  double-dispatch.
- **Session.** Per claim: create the worktree/branch from the issue (the
  same naming + `add_checked` path as branch-from-issue, linked via
  `issue_links`), then dispatch a **headless** agent through the shared
  agent-task engine with a new `TaskKind::IssueImplement` (issue id/number/
  title/body/url + branch/base/worktree vars; template under
  `[autopilot.prompts]`). The prompt carries the **merge-queue family's
  rules: commit on this branch, do NOT push, never merge** — the agent holds
  no forge role at this stage.
- **PR.** On agent success (clean status, commits ahead of base), **thegn** —
  not the agent — pushes the branch (a plain push to a new remote branch,
  never any force) and creates the pull request through the forge seam,
  title/body derived from the issue with a provider-appropriate closing
  reference, `open_as = "ready"` (or `"draft"`). The PR is then **enqueued
  into the PR queue**, whose existing loop owns everything after — re-runs /
  fixes CI, resolves conflicts, answers review feedback, and lets the forge
  merge under branch protection. THE-56's "responds to review comments,
  re-runs CI, pushes follow-ups" is the PR queue's existing remit; autopilot
  adds the front half and the bridge.
- **Status sync.** Claim ⇒ tracker status `in_progress` (plus an optional
  linking comment, `comment_on_pickup`, default off). PR merged (observed by
  the queue) ⇒ `done` when `done_on_merge` (default on, autopilot-started
  runs only). Failure/timeout/dirty result ⇒ run marked `needs_human` with a
  notification and **no** tracker write — a human decides what a stuck issue's
  status is.
- **Surfaces.** `thegn autopilot` CLI namespace (`status`, `stop <issue>`,
  `retry <issue>`; `--json`) — each verb a `thegn_core::capability::CATALOG`
  row gated by `required_scope`, never a second policy table. Autopilot
  state rides existing chrome: the Issues/Mine rows gain a run badge, the
  pr-queue section shows the shepherded PR, and notifications fire on
  picked-up / PR-opened / needs-human / done. Everything is inert while
  disabled.

## Impact

- Roadmap: **Q 212** (task→worktree→agent→review→merge pipeline) is this
  change's spine; **Q 211/215** (task creation/queue) get their tracker-fed
  form; **AA 342/343** (issue↔worktree linkage, move status on merge)
  land the acting half; composes **Z 759** (PR queue) and **Z 335**
  (create PR from worktree); complements **T 758** (merge-queue driver).
- Specs: new `autopilot` capability. `state-db` — ADDED `autopilot_runs`
  table ⇒ **`user_version` bump**.
- Code: `thegn-core` — `TaskKind::IssueImplement` (+ prompt vars/default,
  validation, pinned-count tests), `config_autopilot.rs`, pure pickup/claim
  policy (`autopilot.rs`: trigger matching, claim transitions, concurrency
  gate — table-tested, 95% gate), DB table + store methods. `thegn-host` —
  pickup hook on the issue-refresh completion path, run driver
  (worktree-create → `agent_run` → push/create-PR → enqueue), `cmd/autopilot.rs`,
  notifications, row badges.
- Config: `[autopilot]` table (`enabled`, `trigger_label`, `assignee`,
  `pickup_status`, `max_concurrent`, `max_attempts`, `open_as`,
  `comment_on_pickup`, `done_on_merge`, `agent`/`agent_command`,
  `[autopilot.prompts]`) — every key documented in
  `config/config.toml.example`.
- Help: new `docs/help/autopilot.md` claiming the CLI verbs' action ids and
  any panel badge context (help + prose ratchets).
- In-flight overlap: **depends on** `add-pr-queue` (the back half) and
  `add-agent-task-engine` (dispatch; implemented). Composes with
  `add-issue-driven-worktrees` (same worktree/naming primitives — whichever
  lands second reuses the other's `issue_branch_name`); defers tracker
  status _transitions_ to `add-generic-tracker-model` where available
  (until then `IssuePatch.status` suffices — autopilot writes only
  `in_progress`/`done`). Does **not** build on `add-fleet-view` (depends on
  the excised LLM proxy) and claims no `fleet` naming. The MCP write-tools
  scope-gating work in flight is orthogonal: autopilot's verbs project the
  catalog like any CLI surface.

## Non-goals

- **A tracker webhook/event listener.** Pickup rides the existing poll
  cadence; push-based triggers are a later provider-seam capability.
- **Cross-host claims.** The claim ledger is the local state DB; two thegn
  instances on different machines watching the same tracker can race. Single
  host is the supported shape (the cross-host analogue is
  `add-cross-host-merge-queue`'s territory).
- **Multi-issue planning, task decomposition, or best-of-N.** One issue ⇒
  one worktree ⇒ one agent run ⇒ one PR (Q 219/224/225 remain future work).
- **Autonomous merge or approval.** The forge merges under its own rules via
  the PR queue's existing `merge_mode`; autopilot never widens that.
- **Acting on issues not explicitly marked.** No label ⇒ no pickup; the
  trigger label + assignee policy is the consent token, not a heuristic.
