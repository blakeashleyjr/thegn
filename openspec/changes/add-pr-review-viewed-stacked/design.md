# Design

## Viewed-state cache (state-db)

A new `pr_file_views` table keyed by `(worktree, pr_number, file_path)` with a
`viewed_at` timestamp records which files a reviewer has marked viewed. This
mirrors the existing `issue_links` worktree-scoped-metadata pattern. `user_version`
bump; migration is additive (absent table ⇒ nothing viewed, today's behavior).

## GitHub sync (git-backend / svc)

GitHub tracks per-file "viewed" state on a PR review. The `GhBackend` gains two
operations: read the current user's viewed files for a PR (folded into the
existing GraphQL PR query where possible) and mark a file viewed/unviewed. Local
marking writes the cache immediately (instant UI) and syncs to GitHub off-loop;
on PR refresh, GitHub's viewed set reconciles into the local cache (GitHub wins on
conflict, since it is the shared source). Sync failures degrade gracefully — the
local viewed state still works offline.

## Stacked / commit-by-commit walker (panel)

`PanelUi` gains a `pr_commit_idx` cursor over `PanelData.commits`. In stacked
mode, the panel renders the single-commit diff (`git diff <commit~1>..<commit>`)
instead of the whole-PR diff; keys step the cursor. The diff is produced through
the existing `diff_sbs`/panel rendering path — only the range changes. A toggle
switches between squashed (whole-PR) and stacked (per-commit) views.

## Rendering

Viewed files get a glyph/dim in the file list (a `caps::active_glyphs()` symbol,
ASCII fallback). Marking viewed or stepping a commit is a **chrome `dirty`**
repaint of the panel, never a pane recompose. render_plan invariants unchanged.

## Invariants

- **Event loop**: GitHub viewed-sync runs off-loop (the PR-refresh path already
  does), result handed back over the channel + `TerminalWaker`; no polling timer.
- **Render**: panel-only chrome repaint for viewed glyphs + commit stepping.
- **State**: `user_version` bump for `pr_file_views` (additive).
- **Additivity**: pure review UX; no proxy/agent dependency.

## Alternatives considered

- **Local-only viewed state (no GitHub sync)** — rejected; the value is that
  progress matches the web UI and teammates. Local cache is the fast path; GitHub
  is the shared truth.
- **Storing viewed state in the PR JSON cache blob** — rejected; a dedicated table
  is cleaner to query and survives PR-cache invalidation.
- **Always stacked** — rejected; squashed whole-PR review stays the default, with
  stacked as an opt-in toggle.
