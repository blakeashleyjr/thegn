# Generalize the single-issue tracker into a multi-tier work-tracking model

## Summary

superzej today models external trackers as a flat list of `Issue`s behind a
single `IssueBackend` trait. This change generalizes that into a capability-gated
`TrackerBackend` provider model with a richer datamodel: a `Project`/`Epic` tier,
a `Cycle`/sprint tier, a `WorkItem` (superseding `Issue`) carrying `kind`,
`parent_id`, `cycle_id`, estimates and custom fields, plus first-class status
_transitions_ and relation kinds. Linear, Jira, GitHub Issues/Projects v2, and
GitLab all plug in behind the existing `IssueRouter` and degrade gracefully via
declared capabilities rather than faking unsupported features. Worktree binding
gets generalized across providers (auto "In Progress" on worktree-create) and the
panel gains two-way write-back and a kanban board, with worktree checkpoints
syncing through to the linked item.

## Impact

- **AA** (Linear / issues) — items 749, 750, 751, 752, 753, 754, 755, 756, 757, 758.
- **AA** (Linear / issues) — refines the existing single-issue model in items 341–348.
- **AT** (CI / providers) — aligns the capability-gate pattern with items 645, 646, 648, 650, 651.
- Related work item 655 (cross-provider provider:key routing).

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
- No new provider SDKs beyond the named four (Linear, Jira, GitHub, GitLab); other
  providers remain future work.
- No bidirectional _project/cycle_ creation from superzej — write-back in this
  change is scoped to work-item comments, status transitions, and assignee.
- No change to PR/diff/CI panels beyond the shared capability-gate alignment.
