# Worktree tab tree — design

Date: 2026-06-04
Status: approved (user: "Yes, fully implement")

## Problem

Each worktree now owns multiple zellij tabs (the `{base} ·N` pages created by
`thegn new-tab`), but the UI still treats every tab as a flat "branch":

- The sidebar lists `sz-warm-maple ·2` as if it were another worktree.
- The center tabbar strip shows all of the repo's branch tabs, with `·N`
  pages mixed in flat.

The model is **repo → worktree → tabs**; the UI should reflect it.

## Decisions (from user Q&A)

1. **Tabbar**: show only the focused worktree's tabs (its pages) —
   switching worktrees/repos is the sidebar's job.
2. **Sidebar**: worktree row = its base tab; page child rows (`·1`, `·2`, …)
   appear ONLY when a worktree has more than one tab.
3. **Home**: explicit `home` child row, first under each repo — home is the
   main checkout's worktree, a sibling of the others, with its own pages.
   The repo row stays a clickable shortcut to the home tab.

Target sidebar shape:

```
WORKSPACES
──────────────────
▌ thegn
  ├ home
  ├ sz-warm-maple
  │   ├ ·1
  │   └ ·2
  └ sz-bold-pine
  + worktree
○ other-repo
+ new workspace
```

## Approach

Pure plugin-side regrouping (no binary, DB, layout, or keybind changes):
tab names already encode the hierarchy (`{slug}/{branch}[ ·N]`) and both
plugins receive `TabUpdate` live. Rejected alternatives: a binary-supplied
tree (`list --tree` — adds a subprocess hop for data the plugin has) and
renaming tabs to `{slug}/{branch}/N` (touches every name-derived path).

**Shared parsing rule** (duplicated in both plugin crates, like `split_tab`):
`repo` = before first `/`; remainder minus a trailing ` ·N` = `worktree`
base; `page` = N, or 1 when no suffix. Mirrors the binary's
`strip_page_suffix` (suffix must be all digits to count).

## Sidebar (plugin/sidebar)

- `RepoView` gains `worktrees: Vec<WorktreeView { label, pages, active }>`
  with `pages: Vec<PageView { n, position, active }>`; home (label `home`)
  first, then branch worktrees ordered by their lowest tab position
  (stable); pages sorted by page number.
- `Row` becomes `Repo(vi) | Worktree(vi, wi) | Page(vi, wi, pi) |
AddWorktree(vi) | AddNew`. Page rows are emitted only when
  `pages.len() > 1`.
- Selection: `Worktree` → its page-1 tab (lowest page if ·1 was closed);
  `Page` → that tab; `Repo` → home shortcut / open closed repo (unchanged).
- Rendering: worktrees get `├`/`└` connectors as branches do today; page
  rows are indented beneath (`│   ├ ·1` under a non-last worktree,
  `    ├ ·1` under the last); cyan accent on the active worktree and the
  active page row.

## Tabbar (plugin/tabbar)

- Group = the focused tab's `(repo, worktree)`; the strip renders one chip
  per page of that group: `1`, `·2`, `·3`, … (active chip filled cyan,
  click switches, same span mechanics). With one page the strip shows the
  single chip `1`.

## Unchanged

Alt+t / new-tab flow, tab names, resolve/DB, layouts, keybinds, statusbar,
panel.

## Testing

- Plugin unit tests for the parse/group helpers (pure functions, host
  `cargo test` in each plugin crate).
- `test/nav-ux.py` additions: with `wt`, `wt ·2`–`·4` open, drive the
  sidebar by keys (Alt+h, j/k, Enter) and assert each row lands on the
  expected tab (`dump-layout` focused tab name): worktree row → base tab,
  page rows → their `·N` tabs, home row → home tab.
- Existing suites stay green (one-session, smoke, nav-ux, cargo test,
  clippy, fmt for binary + both plugin crates).
