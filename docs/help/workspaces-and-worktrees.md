---
id: workspaces-and-worktrees
title: Projects & worktrees
order: 2
actions:
  [
    new-worktree,
    new-project,
    new-workspace,
    delete-project,
    close,
    close-worktree,
    close-tab,
    switch-project,
    switch-workspace,
    next-worktree,
    prev-worktree,
    next-project,
    prev-project,
    new-worktree-from-template,
  ]
---

# Projects & worktrees

The two core objects. A **project** is a repo; a **worktree** is a git
worktree inside it, shown as a tab. git is the source of truth — thegn's
database only caches and resurrects what git already knows.

## Creating

- `Alt-W` — **new project**: pick a repo from your scanned roots
  (`repo_roots` in the config) or paste a git URL to clone and open.
  A repository reached through multiple roots, such as symlinked or nested
  roots, is listed once.
- `Alt-w` — **new worktree**: branches off the base branch
  (`base_branch = "auto"` follows the current branch), names it with your
  `branch_prefix`, opens a tab, and asks what to run: a coding agent from
  `[[agents]]`, a tool from `[[tools]]`, or a plain shell — optionally
  inside a sandbox. Configured `pre_create` hooks run before git creates it;
  `post_create` hooks run after registration and existing provisioning.
- Saved `[[worktree_templates]]` presets (and existing tmuxinator/sesh
  project files) appear in the "what to run" picker and the
  [[command-palette]].

With `[git] submodules = "auto"` (the default), a clone or newly-added
worktree that has a root `.gitmodules` initializes its submodules recursively
on the existing background creation worker. Provider and remote
materialization follow the same rule. Initialization failure is non-fatal: the
worktree stays registered and usable, and the progress/error notice says that
submodules were not initialized. Set the mode to `"off"` to skip recursive
clone, initialization, and deeper state reads.

Before initialization, thegn normalizes the configured path/URL pairs and uses
the repository-trust approval flow for a distinct submodule request. Denial or
an unavailable approval never enables `protocol.file.allow` and never blocks
the checkout itself. The mode is trusted user/workspace configuration; a repo
cannot turn it on through its own `.thegn.toml`.

A pasted git URL is cloned into a private staging directory beside the
destination, then renamed into place only after the clone validates. A failed
or cancelled clone leaves no destination; a destination with an origin but no
HEAD is refused as an incomplete clone (`remove it and retry`).
If a clone crashed and left its staging directory behind, a later clone to the
same destination reclaims it only when its process is gone and the directory
is owned by you. Live clones, symlinks and other paths are left alone, and a
failed cleanup does not fail the new clone.
While cloning, thegn holds an owner-only `.<name>.clone-claim` file beside
the destination with the process id and a generation. A second clone is
refused before Git work while the first process is alive; a claim left by a
dead process is reclaimed once. Symlinked, unreadable or unowned claims are
refused with instructions to remove them if no clone is running. The claim is
removed when cloning ends.

A clone keeps the destination's parent directory open while it runs and
promotes the staged clone relative to that directory without replacing an
existing destination. If the parent is replaced during the clone, promotion
is refused and the staging directory is left for the startup sweep. A
configured workspace directory that is itself a symlink still works; thegn
resolves it first. The parent directory must be owned by you.

A URL clone records where it came from: the credential-free canonical origin, the thegn version and when
the clone started and was validated. `thegn repo provenance` lists the recorded clones newest first,
`thegn repo provenance <path>` shows one, and `--json` emits one JSON array. Each row carries a status
read at that moment: `verified` (the directory exists and its `origin` still matches),
`origin-changed`, `missing` (the directory is gone), `unverifiable` (the origin could not be read),
`cloning`, `interrupted` (the clone's process is gone before it finished) and `abandoned` (the clone
failed or was cancelled). The record never contains the clone URL's credentials and is advisory: the
repository on disk stays authoritative.

At startup, a `cloning` record whose process is gone or was never recorded is
marked `abandoned`, so `repo provenance` no longer reports it as `interrupted`;
a live foreign clone is left alone.

Lifecycle hooks apply to every creation door, including the wizard, `wt new`,
issue dispatch, and the daemon's `worktrees.create` request. A blocking
`pre_create` leaves both git and the database unchanged. The wizard schedules
the default asynchronous `post_create` work before its completion event; a
`wait = true` entry gates the first pane. Headless CLI creation waits for its
post-create job before returning, while daemon creation reports the existing
worktree response and completes post-create asynchronously.

## Lifecycle and sessions

`pre_destroy` runs before provider teardown and git removal, and `post_destroy`
runs from the repository root after a successful removal. User deletion keeps
the worktree visible when a blocking pre-destroy hook fails; the existing
delete confirmation supplies the force/delete-anyway path. Explicit workspace
cleanup and wizard rollback use force semantics so speculative worktrees do
not leak. Automatic merge reclaim is unattended: it reports hook failures and
continues only after the existing clean guard.

`session_start` runs once when the first pane for a worktree session is about
to spawn, and `session_end` runs once when the last pane exits or its tab
closes. Both are asynchronous and never delay pane creation or tab close. See
[[configuration]] for the event lists, working directories, environment, and
repo trust behavior.

## Switching

- `Alt-↑/↓` — move to the pane above/below; at the top/bottom pane, with no
  pane left in that direction, it switches to the previous/next worktree within
  the project. It never focuses the top/bottom bars — that is `Ctrl-↑/↓`.
- `Shift-Alt-↑/↓` — previous/next project. One ring: the projects in
  sidebar order, then the terminal hosts, wrapping across the boundary.
  Projects and hosts you have **collapsed** are stepped over — a folded group
  is one you are not working in, so navigation neither stops on it nor expands
  it. Set `[ui] sidebar_nav_skips_collapsed = false` to visit every group.
- `Alt-o` — project switcher; `Alt-1..9` / `Ctrl-1..9` jump by sidebar
  slot; the palette's `~` mode ranks everything by frecency. `Ctrl-<digit>`
  needs a terminal that reports modified keys — [[terminal-compatibility]]
  covers what to do when it does not, and `Alt-o` always works.

Switching to a project that is not already warm shows an `Opening <name>…`
spinner on the status line while its saved state loads off the UI thread; a
warm project switches immediately.
Creating, opening or cloning a project uses this same flow: database and git
work runs off the UI thread, and the status line shows `Opening <name>…` before
the project opens. Deleting the project you are in switches to the first
remaining project in sidebar order (immediately if warm, otherwise with the
spinner); deleting the last project leaves an empty view.

The canonical action ids above use `*-project`. Existing bindings using
`new-workspace`, `delete-workspace`, `switch-workspace`, `next-workspace`,
`prev-workspace`, or `summon-workspace` remain accepted as compatibility
aliases; their chords do not change.

## Closing and deleting

- `Alt-x` closes the focused pane if the tab is split, else the tab.
- `Alt-X` removes the worktree and its tab; **the branch is kept**.
- Deleting from disk is always an explicit second step in the [[sidebar]]
  delete menu — nothing destructive rides on a single keystroke.

From the sidebar delete menu, the worktree row and its pipeline rows disappear
at once, and focus moves on. If removal fails, the row returns in place and
the status line says `Worktree delete failed for <path>: <reason>`.

While a worktree delete is in flight, jumping to that worktree from the
sidebar, a notification, the command palette, or connect is refused with
`'<name>' is being deleted`. An adopt into it leaves the session headless with
`adopt: <path> is being deleted — session left headless`.

New worktrees live under `~/.thegn/worktrees/<repo>/<label>--<digest>` by
default; `worktree_mode = "in_repo"` keeps them in `<repo>/.worktrees`. The
label is a readable slug of the branch; the 64-hex digest is derived from the
repository's Git common directory plus the exact branch name, so `feat/a`,
`feat-a` and `feat_a` — or two repos that share a basename — never share a
checkout. Existing worktrees are never moved; they stay where Git has them. A
destination that already exists is refused rather than reused, and a rename
whose `git worktree move` fails rolls the branch rename back. Each rename
writes an owner-only intent record in the repository Git directory; the next
rename or worktree creation in that repository, and a check at every launch,
resolve an interrupted move or refuse an ambiguous state, keep the record and
show the outcome in a launch toast. A path that is not a registered worktree on
the old branch is refused. Before creating a worktree, thegn checks whether
its branch already exists; if Git cannot verify that (for example, because
it times out), creation is refused without changing anything. Failed branch
renames and worktree moves include Git's reason.
The same fail-closed rule covers branch, worktree and base-branch reads:
`thegn wt new` (including batched and issue-driven creation) and autopilot
refuse when Git cannot be read instead of assuming no branches exist or
silently choosing a default. A detached HEAD remains valid and uses the
default branch as its base.
The new-worktree wizard follows the same fail-closed rule in its resolve-base
step: if Git cannot list branches or resolve the base, the step fails with
Git's reason before anything is created.

Worktree tabs are named `<repo>/<exact branch>`. See the [[config-reference]]
for every knob.

### Pruning merged worktrees

`thegn wt prune --merged` removes lane worktrees whose branch is already merged
into the target, together with those branches. It is a dry run by default: it
lists what would be removed and why every other worktree is kept; add `--apply`
to remove. `--target <branch>` overrides the target (default: the repo's merge
target). `--json` emits one JSON object. A bare `wt prune` is refused.

A worktree is kept when it is the primary checkout, holds the target branch,
has a detached HEAD, is not merged (or Git cannot say), has a branch tip equal to
the target (nothing worked on it yet), has uncommitted or
untracked files, has a process running inside it (Linux only). Live sessions and merge-queue
rows are not checked. Each worktree is re-checked just before its removal and
skipped if it changed meanwhile. A removal that fails is reported and the rest continue; the exit
status is non-zero if any failed.
