# Add issue-driven worktrees (issue → worktree → agent)

## Summary

Close the loop from the existing unified-work surface to execution: from an issue
in the work panel, one action **creates a worktree-tab for it** and — optionally —
**launches an agent seeded with the issue's context**. Borrowed from
[`emdash`](https://emdash.sh) and [`jmux`](https://github.com/jarredkenny/jmux),
whose headline flow is "pick a Linear/GitHub issue, press a key, get a
worktree + session + agent seeded with the issue." Config knobs
`auto_create_worktree` / `auto_launch_agent` / `session_name_template` control how
far the automation goes.

## Impact

- **Q 211** (create task from a prompt/spec) / **Q 212**
  (task→worktree→agent pipeline) — this is the entry step of that pipeline.
- **Unified work surface** (IssueRouter / `Section::Mine` / `WorkRow`) — turns the
  read-only issue feed into an actionable one.
- **D** (worktrees) — creates and reveals a worktree tab from an issue.
- Extends the `navigation` and `agent` capabilities. **No DB schema change** — the
  issue↔worktree binding reuses the existing `issue_links` table.

## Rationale

thegn already aggregates issues across providers (`IssueRouter`, Linear/GitHub/
Jira), renders them in the My Work panel with a linked-worktree marker, and has
the primitives to create a worktree (`worktree::branch_name`/`add_checked`), add a
tab (`session::add_group`), and launch an agent (`agent::launch_spec`). What's
missing is the one action that composes them and the seeding of issue context into
the agent. emdash frames this as "the only parallel-agent app with issue-tracker
integration, bridging task management and orchestration" — thegn's
worktree-per-tab model makes the binding natural. Keeping the two halves separate
preserves the core invariant: **issue→worktree is shell-level (AI-free); the agent
auto-launch is the additive AI layer** and is skipped entirely when no agent is
configured.

## Non-goals

- **Building an issue tracker** — thegn consumes issues via the existing
  IssueRouter; this change is the action on top.
- **Requiring an agent** — with `auto_launch_agent` off (or no agent configured),
  the action just creates and opens the worktree tab; the shell path stands alone.
- **Automated review/merge of the result** — that is the review-gate / team-fanout
  changes; this change ends at "worktree created (and optionally agent launched)."
- **AI-free-shell dependency** — the worktree-creation half never depends on the
  AI layer.
