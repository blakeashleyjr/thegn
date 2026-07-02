# Add PR review: viewed-state sync + stacked commit-by-commit review

## Summary

Bring two review affordances from [`lumen`](https://github.com/jnsahaj/lumen)
into the PR panel, both pure human-facing git/GitHub UX (no AI):

1. **Per-file "viewed" state, synced to GitHub** — mark a file reviewed; the state
   persists locally and syncs to GitHub's native PR "viewed" flag, so review
   progress survives restarts and is shared with the web UI and other reviewers.
2. **Stacked / commit-by-commit review** — walk a PR one commit at a time
   (`git diff <commit~1>..<commit>`) instead of only the squashed whole-PR diff,
   so a reviewer can follow the author's intended steps.

## Impact

- **T 259–270** (review/merge track) — concrete review-panel behavior: viewed
  progress + per-commit walking, both feeding the existing approve/merge flow.
- Extends the `panel` and `git-backend` capabilities and the `state-db` capability
  for the viewed cache. **DB schema change: `user_version` bump** (a `pr_file_views`
  table keyed by worktree + PR + file path).

## Rationale

superzej already has a worktree-scoped PR panel, a GraphQL PR query + cache, and
per-branch PR mapping. What it lacks — and lumen ships — is (a) tracking which
files you've reviewed, synced to GitHub's own "viewed" checkbox so progress isn't
lost on restart and matches the web UI, and (b) a stacked view that walks commits
individually. superzej's SQLite cache is well-suited to the viewed state (the
existing `issue_links` table is the precedent for worktree-scoped review
metadata), and `PanelData.commits` already carries the commit list a walker needs.
Both are pure review UX and stay entirely in the AI-free shell.

## Non-goals

- **Authoring inline review comments from the agent** — that is the separate
  agent-steerable-review change; this change is human viewed-state + commit walking.
- **A full local review database / offline PR mirror** — only viewed-state is
  cached; PR content still comes from the existing GraphQL cache.
- **Non-GitHub forge viewed-sync** — GitHub first (the existing `GhBackend`);
  GitLab/others follow the same local-cache pattern later.
- **Any AI dependency** — pure git/GitHub review; no proxy/agent involvement.
