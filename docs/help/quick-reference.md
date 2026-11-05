---
id: quick-reference
title: Quick reference
order: 2
actions: []
---

# Quick reference

These are the default chords. `Ctrl-Space` opens the [[command-palette]] with
every action. [[keybindings]] is generated from your config, including rebinds.
`Alt-x` means hold Alt and press x; `Alt-X` means Alt+Shift+x.

## Move around

| Keys                          | Does                                                                                            |
| ----------------------------- | ----------------------------------------------------------------------------------------------- |
| `Alt-←` / `Alt-→`             | Focus the neighbouring pane, then sidebar or panel; at an outer edge, previous / next tab       |
| `Alt-↑` / `Alt-↓`             | Focus the neighbouring pane; at an outer edge, previous / next worktree                         |
| `Ctrl-←↓↑→` or `Ctrl-h/j/k/l` | Move focus only; never switch tab or worktree                                                   |
| `Shift-Alt-↑` / `Shift-Alt-↓` | Previous / next project                                                                         |
| `Alt-1..9`                    | Jump to worktree N in sidebar order                                                             |
| `Ctrl-1..9`                   | Jump to project N; needs a terminal that sends modifyOtherKeys (see [[terminal-compatibility]]) |
| `Alt-o`                       | Switch project                                                                                  |
| `Alt-s` / `Alt-.`             | Focus the sidebar / diff and PR panel                                                           |
| ``Alt-` ``                    | Bounce between the projects and terminals regions                                               |

## Create

| Keys    | Does                                          |
| ------- | --------------------------------------------- |
| `Alt-W` | Create a project by opening or cloning a repo |
| `Alt-w` | Create a worktree, then choose what to run    |
| `Alt-t` | Create a tab on the same worktree             |
| `Alt-T` | Create a terminal tab                         |
| `Alt-p` | Create a smart split                          |
| `Alt-n` | Split down                                    |
| `Alt-N` | Split right                                   |

## Tools and panels

| Keys                    | Does                                           |
| ----------------------- | ---------------------------------------------- |
| `Alt-g`                 | Open lazygit                                   |
| `Alt-e`                 | Open your `$EDITOR`                            |
| `Alt-/`                 | Open the git diff                              |
| `Alt-y` or `Ctrl-Alt-f` | Open the files drawer                          |
| `Ctrl-Alt-s`            | Cycle the sidebar: full, rail, hidden          |
| `Ctrl-Alt-p`            | Show or hide the diff and PR panel             |
| `Ctrl-Alt-z`            | Zoom the focused pane                          |
| `Ctrl-Alt-/`            | Search pane history                            |
| `Ctrl-/`                | Search across panes                            |
| `Ctrl-g`                | Lock keybinds and pass every chord to the pane |
| `F1`                    | Open help                                      |
| `Ctrl-q`                | Quit the UI; daemon-backed panes keep running  |

## Close and quit

| Keys or action       | Does                                                                  |
| -------------------- | --------------------------------------------------------------------- |
| `Alt-x`              | Close the focused pane in a split, otherwise close the tab            |
| `Alt-X`              | Remove the worktree (see Destructive actions)                         |
| `Ctrl-q`             | Quit the UI; daemon-backed panes survive and can reattach on relaunch |
| Palette: `detach`    | Detach while keeping everything running                               |
| Palette: `quit-kill` | End the sessions instead of detaching                                 |

## Destructive actions

These cannot be undone from inside thegn. Read the prompt before confirming.

| Action                                                       | What it does                                                                                                                                                                                                                                              | Safety net                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Alt-X` (remove worktree)                                    | Deletes the worktree checkout directory from disk and closes its tab. The branch stays. The home checkout cannot be deleted. Choosing a delete option loses uncommitted work in that directory.                                                           | With the default `confirm_delete = true`, a clean worktree shows a menu with “delete from disk,” “delete anyway (ignore blocking hooks),” “keep files,” and “cancel.” The first entry, “delete from disk,” is preselected: read the list before pressing Enter. If confirmation is disabled, a clean worktree is deleted immediately. A dirty worktree, or one whose status is not yet known, always shows “Delete N worktree(s) with UNCOMMITTED changes?” with Cancel preselected. |
| Sidebar `d` on a worktree                                    | Opens “Close or delete this worktree?” Close keeps the branch and files. The other choices delete the branch and files, or ignore blocking hooks.                                                                                                         | The chooser always opens. Close is preselected; the two delete choices are marked dangerous.                                                                                                                                                                                                                                                                                                                                                                                         |
| `thegn wt rm TARGET`                                         | Removes the worktree after its destroy hooks. `--force` skips a blocking destroy hook. `--delete-branch` also force-deletes the branch with `git branch -D`, without checking whether it is merged.                                                       | A failed blocking hook leaves the worktree in place and names `--force`.                                                                                                                                                                                                                                                                                                                                                                                                             |
| `thegn land`, `thegn merge land`, or `thegn integrate --all` | `thegn land` folds a branch into the target (default `main`); `thegn merge land` lands an eligible queue row. Both advance the target ref, and a land is awkward to walk back. `integrate --all` folds every eligible branch, including work in progress. | `integrate` prints its plan and confirms first; `--dry-run` previews it. By default, only queued branches fold; `--all` includes every eligible branch.                                                                                                                                                                                                                                                                                                                              |
| `thegn pr merge`                                             | Merges the worktree’s pull request on the forge.                                                                                                                                                                                                          | Confirms the pull request number at its reviewed head first. `--yes` skips the prompt.                                                                                                                                                                                                                                                                                                                                                                                               |
| `rollback` (palette)                                         | Restores a worktree to a prior snapshot.                                                                                                                                                                                                                  | Read [[git-and-diffs]] and the action’s prompt before confirming.                                                                                                                                                                                                                                                                                                                                                                                                                    |
| “Quit and kill sessions”                                     | Ends every daemon-backed session instead of detaching.                                                                                                                                                                                                    | Choose it deliberately; `Ctrl-q` alone leaves daemon-backed panes running.                                                                                                                                                                                                                                                                                                                                                                                                           |

Closing with `Alt-x` is routine; removing with `Alt-X` is deliberate. Git is
the source of truth, so a removed worktree’s branch can be checked out again
with `git worktree add`.

## Where to go next

- [[keybindings]] — every key, with your rebinds
- [[command-palette]] — find and run actions
- [[terminal-and-panes]] — tabs, splits, and panes
- [[workspaces-and-worktrees]] — projects and worktree lifecycle
- [[cli]] — command-line reference
- [[merge-queue]] — queue and land branches
