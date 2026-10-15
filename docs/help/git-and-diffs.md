---
id: git-and-diffs
title: Git & diffs
order: 10
contexts:
  [panel:changes, panel:commits, panel:branches, panel:stash, panel:files]
actions: [git-push, git-pull, git-fetch, rollback]
---

# Git & diffs

The [[panel]]'s **git** tab is a lazygit-style set of sections for the
focused worktree; `Alt-g` opens the real lazygit in a pane when you want
the full tool.

## The git sections

### changes — the working diff

Binary and mode-only changes say so instead of showing hunks. `↵` on a file
opens line-level staging at every width; at Normal width it widens to Half,
and Esc retracts that drill. At Half, the inline preview follows the cursor.
`space` stages, and `e` cycles panel width. Untracked files preview as all
additions and are intent-added (`add -N`) only on the first stage. In staging,
line discard is refused in the STAGED pane (unstage with `space`) and on an
untracked file (`d` on its Changes row deletes the file).
In changes and the commit-files drill, `b` opens blame when available;
untracked and added files are refused, and other unavailable blame states are
reported. An empty stash message becomes `WIP on <branch>`. A commit with
nothing staged is refused before the message box; partially staged changes
and merge states are allowed.

### commits — branch history

At Half and Full width, the detail region follows the cursor and shows the
commit through `git show`, read-only. `↵` opens the commit's files with the
selected file's diff shown read-only. Mark lines with `space` to start a
custom patch; `W` diffs the selection against the mark. Merge commits show
their diff against the first parent. If there are no changed files, the view
says so; if commit files cannot be read, it reports that state. `E` edits
(interactive rebase stopping at the commit); bare `e` is always the panel
width cycle. Rebase `s` (squash), `f` (fixup), and `d` (drop) ask for
confirmation first; `i` plans from the selected commit and refuses merge
ranges. `B` marks the selected commit as a rebase base; press it again to
clear the mark. With a mark, `i` plans the commits after it up to HEAD and
refuses if the mark is HEAD, is not an ancestor, or is gone (press `B` to
re-mark). Without a mark, `i` plans from the selected commit.

### branches

Branches are local: check out, create, and delete them; open-PR badges ride
each row. At Normal width, `↵` widens to Half. The selected branch's recent
commits appear at Half and Full width. `p` pulls and `P` pushes HEAD only;
both refuse when another row is selected. `n` opens the new-branch name input.
The menu's pull and push actions also operate on HEAD only.
With a rebase base marked, `r` rebases the commits after the mark onto the
selected branch.

### stash

Apply, pop, and drop stash entries. At Normal width, `↵` widens to Half to
show the stash diff, including untracked files. It follows the entry by its
object ID and can be scrolled at both Half and Full widths.
Apply, pop, and drop act on the selected entry by object ID even if stashes
were pushed meanwhile; a stash that no longer exists is refused.

### files

The worktree's file tree; `/` filters it (directories stay while any
descendant matches; Esc clears), `↵` previews a file inline, `o` pages it
in bat, `O` opens your editor, `y` reveals it in yazi. An unknown binary
previews as a hex dump of its first 4096 bytes.

Inside the git-family sections (changes/commits/branches/stash — not
files), `?` shows that section's own key cheatsheet only after entering row
mode. A drill's temporary auto-widen retracts when Esc leaves the last drill;
shrinking to resting width returns to the section's home view. Marks, ranges,
and flows follow lazygit conventions. An action key that needs a wider view
**widens the panel and performs the action** in one press (the status crumb
says so); drill detail views at the half width carry a breadcrumb (`changes
› staging · esc back`) so the way back is always visible.

## Submodule pointers

A submodule is shown as the superproject's atomic gitlink, not as a text
directory. Its change row uses the terminal-safe submodule marker, path, and
abbreviated old and new object IDs, followed by a state such as `forward`,
`rewind`, `diverged`, `dirty`, `uninitialized`, or `unavailable`. It never
shows a made-up `+0/-0` diffstat.

Opening the row asks a background worker for a bounded commit summary using
objects already present locally. This view never fetches; when either object is
missing it keeps the two IDs visible and says that the range is unavailable.
`space` still stages or unstages the whole gitlink. Enter does not open line
staging, because submodule pointers are atomic.

Configure lifecycle reads with `[git] submodules = "auto"` (the default) or
`"off"`. `auto` acts only when a root-level `.gitmodules` exists; `off` skips
initialization and deeper state queries. `THEGN_GIT_SUBMODULES` is the matching
environment override. This is trusted user/workspace configuration, not a key
a repository may set through `.thegn.toml`.

`Alt-/` opens a plain `git diff` in a pane; `thegn wt diff` prints one
from any shell.

## Structural diffs (difftastic)

`[git] structural_diff` renders the **read-only** diff surfaces — the
full-screen diff view and `thegn diff --structural` — through
[difftastic](https://difftastic.wilfred.me.uk/) instead of a line diff:

- `off` (default) — thegn's internal unified view.
- `auto` — structural when `difft` resolves (config override → `PATH` →
  a managed download), the internal view otherwise.
- `difft` — always structural; on any failure (tool missing, timeout,
  oversized change) it falls back to the internal view with a one-line
  notice, never blocking.

In the full-screen structural view, **`t`** toggles between the
structural render and the internal unified view. Setting `structural_diff`
also makes `Alt-/` open this native view even when a `[[tools]] diff` is
seeded — an explicit opt-in wins over the default.

In the internal full-screen diff, **`e`** opens the selected file in your IDE;
on an added or context line it includes that new-side line number. A deleted
line has no new-side location, so thegn opens the file and says why. The flat
structural rendering has no stable selected file—press `t` first.

Structural output is **read-only**: it is never fed to `git apply`, and
every _stageable_ diff (inline hunks, line staging) keeps the sanitized
flags regardless of this setting. Acquire/inspect `difft` via
`[managed_tools.difft]` and `thegn doctor` (its **Source-control
workflow posture** section also reports the git version against the
merge-queue fold's floor, jj colocation, and any custom merge drivers).

## Coexisting with jujutsu

In a repo colocated with [jujutsu](https://jj-vcs.github.io/) (a `.jj/`
beside `.git/`), thegn stays out of the way rather than fighting it:
worktree rows carry a jj marker, detached `HEAD` (jj's normal state) is
not shown as an error, and the panel header reads `HEAD (jj working copy)`.
The Full git view shows a `jj: git index ignored` chip, and the Changes section
ends with "jj colocated: staging here does not change jj's working copy."
Successful staging and commit actions add a status-line note that jj ignores
the git index, and background `auto_fetch` skips the repo unless
`[git] auto_fetch_colocated = true`. Reads are unchanged; mutations are
warned about, not blocked.

## Syncing

Push, pull, and fetch act on the **focused worktree**. They have no
default chords — run them from the [[command-palette]], or bind them by
id in `[keybinds]`:

| Action     | Id          |
| ---------- | ----------- |
| Git: push  | `git-push`  |
| Git: pull  | `git-pull`  |
| Git: fetch | `git-fetch` |

Because each worktree is its own checkout, these never touch a branch you
are not looking at — which is what makes running several at once safe.

## Rollback

`rollback` restores a worktree to a prior snapshot when a run goes wrong
— the undo for "the agent made a mess". It is palette-runnable and
bindable like the sync actions.

## Which git does what

thegn reads git natively where it can and falls back to the `git` CLI
otherwise, so nothing depends on a feature the native backend has not
covered yet. Writes always go through the CLI.

One consequence worth knowing: **git is the source of truth**, and
thegn's database is a cache. Deleting a worktree with `git worktree
remove` behind thegn's back is safe — the tree reconciles on the next
launch.

## Landing work

Diffs are where a branch starts; [[merge-queue]] is where it ends. The
fold-actor merges queued branches entirely in the git object database
without checking anything out, so it works even while you are using the
repo. See also [[review-a-pr]].
