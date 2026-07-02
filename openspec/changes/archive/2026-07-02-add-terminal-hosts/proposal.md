# Add terminal hosts

## Summary

Add a "Terminals" section to the sidebar for first-class management of isolated
terminal environments (Local, SSH, Mosh, container) that live outside git
worktrees — so remote infra and scratch shells are managed like worktree tabs
without shoehorning them into dummy repos.

Source design: `docs/superpowers/specs/2026-06-25-terminal-hosts-design.md`.

## Impact

- **J** (Remote access) — SSH/mosh terminal environments as first-class sidebar rows.
- New capability `terminal-hosts`; touches `state-db` (new `terminals` table) and `sidebar`.

## Rationale

superzej relies on git metadata for worktree rows; non-git shells (a remote box, a
local scratch shell) have nowhere to live today. A `terminals` DB table + a
`GroupKind::Terminal` lets them reuse the existing group/tab/pane machinery while
returning empty for git-only queries.

## Non-goals

- Provisioning remote hosts. v1 just connects via `ssh`/`mosh`.
- Replacing worktrees — terminals are a sibling grouping.
