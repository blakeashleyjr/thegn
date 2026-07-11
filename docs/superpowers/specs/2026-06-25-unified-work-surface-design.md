# Unified work surface — design

Status: Slices 1, 2 & 3 landed (multi-provider + "My Work" + worktree binding).
Author: thegn.

> DB note: this branch bumps `SCHEMA_VERSION` to 18 for `my_work_cache`. Sibling
> branches (CI-group `ci_runs_cache`, named-environments `env_name`) also use 18.
> All migrations are additive (`CREATE TABLE IF NOT EXISTS` / `ALTER ADD COLUMN`),
> so the tables coexist; only the version _number_ needs reconciling at merge.
> Supersedes the "single active tracker" assumption in `tasks.md` groups AA/AT.

## Thesis

A developer's work is scattered across tools — Linear for product, Jira for a
client, GitHub issues for OSS; PRs/reviews on GitHub; mentions and CI feedback
everywhere. The tools they reach for are each a **read dashboard for one tool**:

- [`gh-dash`](https://github.com/dlvhdr/gh-dash) — GitHub PRs/issues as
  configurable saved-query sections. GitHub only.
- [`jiratui`](https://github.com/whyisdifficult/jiratui) — a Jira TUI. Jira only.
- Linear's app / [linear](https://github.com/linear/linear) — Linear only.

thegn has a structural advantage none of them has: **the git worktree is the
unit of work**. Each worktree is a tab that already holds the branch, the editor,
the diff, the PR, the CI status, and (via `issue_links`) the tracked issue. So the
maximal play is not "another dashboard" but a **cross-tool, cross-repo work surface
whose every row is one keystroke from a worktree that already contains the work**.

gh-dash/jiratui/Linear can _show_ you work. thegn can _put you in it_.

## What already exists (reused, not rebuilt)

- Tracker-agnostic issue layer: `IssueBackend` trait + per-provider backends
  (Linear GraphQL, Jira REST v3, GitHub `gh`) in `crates/thegn-svc/src/issue/`,
  fronted by `IssueRouter`. Unified `Issue` domain type in
  `crates/thegn-core/src/issue.rs` (carries `provider`, `branch_hint`,
  `project_ids`, `blocked_by`).
- `[issues]` config with per-provider sections and `env:` secret expansion.
- SQLite caches keyed `(repo_root, provider)`: `issue_cache`, `issue_links`,
  `issue_relations`, `issue_projects`.
- A `Work` panel tab (PR / Issues / Problems / Jobs / Tests / Symbols); the Issues
  section already renders a per-row provider sigil and a worktree-link marker.
- Event bus + notification inbox with the right kinds already defined: `Assigned`,
  `Mentioned`, `PrLinked`, `BlockerResolved`, `Overdue`, `PrStateChanged`.
- GitHub PR layer (status/checks/reviews) cached by `spawn_pr_cache_refresh`.

## Gaps

1. **One tracker at a time.** `IssueRouter` held a single `Option<RouterInner>`;
   config exposed a single `provider`. You could not run Jira _and_ Linear at once.
2. **No unified "My Work" view.** Issues/PRs/notifications are siloed and scoped to
   the current worktree's repo — no cross-repo "what needs me now" surface.
3. **Worktree binding is plumbed, not automated.** No human-facing
   branch-from-issue, no move-issue-on-merge.

Discovered while implementing: `HydrateHints.issues_provider` was declared but never
populated at any construction site, so `build_model`'s issue-cache load was dead. The
fix (load all cached providers directly) doubles as the multi-provider read path.

## Slice 1 — multi-provider aggregation (data layer)

- `IssuesConfig` gains `providers: Vec<IssueProviderKind>`. `active_providers()`
  returns `providers` (minus `None`) when non-empty, else falls back to the legacy
  single `provider`. Back-compat preserved.
- `IssueRouter.inner: Vec<RouterInner>` — one backend per active provider.
  `list_issues`/`search` fan out across all and concatenate; `get/update` dispatch by
  the `"<provider>:"` id prefix; `create` targets the first provider. A failing
  provider logs and contributes `[]` — never breaks the others.
- `list_per_provider()` returns per-provider results so the refresh worker can cache
  and diff each provider under its own `(repo_root, provider)` key (no schema change).
- `build_model` loads _all_ cached providers for the repo via the new
  `Db::get_all_issue_cache` and concatenates into `tracker_issues`. The dead
  `issues_provider` hint is removed.
- UI: none — `issues.rs` already renders mixed-provider lists.

## Slice 2 — unified "My Work" surface (cross-repo)

New `Section::Mine` (first in the `Work` tab): a gh-dash-style grouped, cross-repo
actionable list (Assigned to me / Review requested / Needs attention), built from
all providers' assigned issues + `gh search prs --review-requested/--author @me` +
high-priority unread notifications. Cached globally (not per-repo; bump
`SCHEMA_VERSION`). Enter jumps to the linked worktree, else offers branch-from-issue.

## Slice 3 — worktree binding & lifecycle automation

- `Action::WorktreeFromIssue`: create a worktree from `issue.branch_hint` (reusing
  the `NewWorktree`/`add_worktree` path) and `link_issue` it — the human mirror of the
  existing `AgentDispatch` flow.
- Status-on-merge: when `spawn_pr_cache_refresh` sees a PR go `MERGED` and its
  worktree has a linked issue, call `update_issue(IssuePatch{status: Done})`,
  gated by `[issues].move_on_merge` (default off).

## Non-goals

- Multi-forge PRs (GitLab/Gitea/Forgejo) — roadmap group AT, deferred.
- Multiple instances of the _same_ provider kind (two Jira sites) — future.
- Writing a bespoke board/kanban — out of scope.

## Invariants

New background workers follow the channel + `TerminalWaker` pulse pattern; no polling
timeouts; ~0% idle CPU preserved. Core logic carries unit tests against the 95% gate;
I/O seams exercised by `test/smoke.sh`.
