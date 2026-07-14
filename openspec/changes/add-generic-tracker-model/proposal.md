# Generalize the single-issue tracker into a multi-tier work-tracking model

## Summary

thegn today models external trackers as a flat list of `Issue`s behind a
single `IssueBackend` trait. This change generalizes that into a capability-gated
`TrackerBackend` provider model with a richer datamodel: a `Project`/`Epic` tier,
a `Cycle`/sprint tier, a `WorkItem` (superseding `Issue`) carrying `kind`,
`parent_id`, `cycle_id`, estimates and custom fields, plus first-class status
_transitions_ and relation kinds. **Linear is the reference provider** exercising
every capability; Jira and GitHub Issues keep their current methods behind
honest, mostly-false caps and degrade gracefully via `IssueError::Unsupported`
rather than faking unsupported features — all behind the existing `IssueRouter`.
Named multi-account Linear instances (`linear@<account>`) route item ids
context-free, worktree binding gets generalized across providers (auto
"In Progress" on worktree-create), the panel gains two-way write-back and a
grouped by-status view, a `HouseTracker` MCP tool family exposes the tracker to
agents (writes off by default), and worktree checkpoints sync through to the
linked item.

## Impact

- **AA** (Linear / issues) — items 749, 750, 751, 752, 753, 754, 755, 756, 757, 758.
- **AA** (Linear / issues) — refines the existing single-issue model in items 341–348.
- **AT** (CI / providers) — aligns the capability-gate pattern with items 645, 646, 648, 650, 651.
- Related work item 655 (cross-provider provider:key routing).
- **State DB** — `SCHEMA_VERSION` 43 → 44: new `tracker_meta` and
  `issue_detail_cache` tables; the existing dormant `issue_projects` table is
  activated (no new project table). The migration clears `issue_cache`/
  `issue_projects` rows (pure caches, rebuilt by refresh).
- **MCP house tools** — a `HouseTracker` trait joins `HouseGit`/`HouseForge`/
  `HouseMerge`; write tools are gated by `[issues] agent_write` (default off).

## Rationale

The capability-gate already exists for CI providers (`CiProvider::caps() ->
CiCaps`, `status_raw` raw-name preservation, static-dispatch `CiClient`). This
change applies the identical pattern to trackers so a thin provider (e.g. GitHub
Issues without Projects) advertises `TrackerCaps { projects: false, .. }` and the
chrome hides what it cannot do, instead of synthesizing fake projects/cycles. The
work reuses the existing `IssueRouter`/`RouterInner` fan-out (including
`list_per_provider` for cache diffing), the `Issue.project_ids`/`blocked_by`
fields, `issue_relations.kind`, the `RefreshKind::Issues` hydration path, and the
`Section::Issues`/`Section::Mine` panel sections — so the substrate is additive,
not a rewrite.

## Non-goals

- No AI/agent dependency: trackers are an AI-free shell capability; the existing
  `AgentDispatch` linkage stays optional and additive and the tracker model MUST
  function with all AI layers absent.
- Linear is the only provider completed to full capability in this change;
  completing Jira and adding GitHub Projects v2 / GitLab providers are follow-up
  changes, and other providers remain future work.
- No kanban board view — this change ships a grouped by-status view; the board
  is deferred to the follow-up change `add-tracker-board-view`.
- No OAuth flow — `thegn tracker login` stores pasted, validated API keys; OAuth
  arrives later behind the same seam.
- No bidirectional _project/cycle_ creation from thegn — write-back in this
  change is scoped to work-item comments, status transitions, and assignee.
- No change to PR/diff/CI panels beyond the shared capability-gate alignment.
