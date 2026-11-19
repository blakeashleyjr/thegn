# Sidebar short worktree list

## Why

A group with dozens of agent lanes buries the worktrees that matter. The
sidebar should show a short list, keep what needs attention, and fold batches.

## What Changes

- `[ui] sidebar_group_row_limit` defaults to 7, with protected rows never folded.
- New `[ui] sidebar_batch_group_min` folds worktrees sharing a batch token into one row.
- A worktree row never repeats its title on the second line.
- The tree guide is continuous through two-line rows and open groups.
- Worktree names are toned by state.

## Impact

Sidebar rendering, `[ui]` config, help page `sidebar`.
