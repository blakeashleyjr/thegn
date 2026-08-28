---
id: panel
title: Diff / PR panel
order: 5
contexts:
  [
    zone:panel,
    panel:mine,
    panel:across,
    panel:issues,
    panel:problems,
    panel:jobs,
    panel:tests,
    panel:symbols,
    panel:notifications,
    panel:logs,
    panel:hosts,
    panel:telemetry,
    panel:debug,
    panel:db,
  ]
actions: [focus-panel, toggle-panel]
---

# Diff / PR panel

The right panel tracks the focused worktree. `Alt-.` (or `Ctrl-→` from the
rightmost pane) focuses it; `Ctrl-Alt-p` hides it. It is a tabbed
accordion — four tabs, one open section at a time:

- **git** — changes (diff), commits, branches, stash, files
- **work** — mine (your unified work feed), across (cross-worktree
  attention), the branch's PR + the repo's open PRs, CI runs, the merge
  queue, the PR queue, issues, problems, jobs, tests, symbols
- **system** — notifications, logs, sandbox, hosts, environments, shares,
  port forwards, telemetry, media, keys — plus two **reserved** stubs,
  `debug` and `db` (see below)
- **help** — this documentation, docked (the twin of the `F1` overlay)

## Keys

- `Tab` / `Shift-Tab` — cycle tabs; number keys jump to the active tab's
  Nth section
- `j k` / `↑↓` — walk the open section's **rows** (the item-first model:
  the first press drops straight into the rows)
- `Shift-J` / `Shift-K` — hop between section headers
- `↵` — act on the cursor row (open, jump, link — per section below);
  `Esc` steps back out of the rows
- `e` — cycle the width: normal → half → full-screen; `E` cycles the other
  way (shrink). Exception: in the git **commits** view `E` belongs to the
  git table (edit/reword) — use `e` to keep cycling round.
- `F1` — open help for the focused section (git-family sections also show
  their own gitui cheatsheet on `?`)

Each section documents its own keys in the status bar while it has focus,
and [[keybindings]] lists them all under a `Panel · <section>` heading.

`[panel] sections` reorders the accordion or hides sections you never
use; the built-in order is the default.

## What each section shows

**git**

| Section    | Shows                          |
| ---------- | ------------------------------ |
| `changes`  | the working diff, hunk by hunk |
| `commits`  | branch history                 |
| `branches` | local and remote branches      |
| `stash`    | stash entries                  |
| `files`    | the worktree tree              |

**work**

| Section    | Shows                                                                 |
| ---------- | --------------------------------------------------------------------- |
| `mine`     | one feed of everything assigned to you: issues, review requests, PRs  |
| `across`   | failing CI across **all** worktrees, grouped by worktree (read-only)  |
| `pr`       | PR state, CI check rollup, review decision for this branch            |
| `ci`       | run history and per-run state across providers                        |
| `merge`    | the local merge queue — per-branch land/defer status                  |
| `prq`      | the PR queue — queued pull requests on the forge and what blocks them |
| `issues`   | tracker issues                                                        |
| `problems` | compiler, linter, and test diagnostics                                |
| `jobs`     | configured shell jobs (build, test, run)                              |
| `tests`    | test results and the pass/fail rollup                                 |
| `symbols`  | the LSP / tree-sitter outline for the selected file                   |

**system**

| Section         | Shows                                                     |
| --------------- | --------------------------------------------------------- |
| `notifications` | the notification list (see below)                         |
| `logs`          | thegn's own log stream                                    |
| `sandbox`       | live sandbox state for this worktree — see [[sandboxing]] |
| `hosts`         | configured `[host.*]` machines and their state            |
| `environments`  | configured `[env.<name>]` environments                    |
| `share`         | ports this worktree exposes — see [[share-and-forward]]   |
| `forward`       | auto port forwards to host loopback                       |
| `telemetry`     | live frame/loop counters for the running UI               |
| `media`         | now-playing and transport — see [[media]]                 |
| `keys`          | the effective keymap, same as [[keybindings]]             |

`hosts` and `environments` are **dev-channel only**; see
[[release-channels]].

The merge queue drives the local fold-actor, and the PR queue shepherds
pull requests on the forge — see [[merge-queue]] and [[pr-queue]] for
those workflows.

## Width: memory, DWIM, config, drag

- **Per-section memory** — each section remembers the width you last gave
  it (persisted); hopping sections restores it, so a full-screen Logs and a
  narrow Changes coexist.
- **Auto-widen (DWIM)** — a git action that needs a wider view (e.g. `d`
  diff at the resting width) widens the panel and performs the action in
  one press, instead of printing a "widen first" note.
- **`[panel] width` / `half_ratio`** — the resting column count and the
  half-screen fraction are configurable (see the configuration reference).
- **Drag** — at the resting width, drag the panel's left separator to
  resize; the width persists and becomes the new resting width.

## The work tab, section by section

### mine — your unified work feed

Everything waiting on _you_, aggregated across tools and grouped as
**Review requested** · **Needs attention** · **Assigned to me**: issues
assigned to you (every configured `[issues]` provider), PRs where your
review is requested or that you authored (via `gh search`), and
high-priority notifications. Scoped to the active repo by default; `a`
widens to every repo. A `◈` marks a row already linked to a worktree.

Keys: `↵` open in browser · `b` branch a worktree from the issue · `o`
browser · `a` this repo ↔ all repos · `R` refresh.

If the repo has no `origin` remote the repo-scoped PR search cannot run;
the section says so rather than silently searching every repo.

### across — cross-worktree attention

A read-mostly stream of things needing attention in your _other_
worktrees — currently each worktree's failing CI (latest run per
workflow), grouped by worktree. Scoped to the active workspace by
default; `a` widens to every workspace. `↵` jumps to that worktree's tab.

### pr — the branch's PR, and the repo's open PRs

The top shows the current branch's PR: state, CI check rollup, review
decision, and unresolved review threads. Below it, **OPEN PRS** lists the
repo's other open PRs (`◈` = that branch already has a local worktree),
so the section answers "what's open on this repo", not only "what's open
on this branch".

Keys: `M` merge · `A` approve · `c` comment · `r` rerun failed checks ·
`o` open in browser. See [[review-a-pr]] for the full workflow.

### ci — pipeline state

One row per workflow, judged by its most recent run — the same set the
header counts, so the `✓/✗` numbers always match the rows. Fan-out
workflows (several sibling runs from one trigger) are represented by a
failing sibling when there is one. History is behind the drill-in.

Keys: `↵`/`v` drill into the run · `o` browser · `r`/`R` rerun (all /
failed) · `c` cancel · `g` force-refresh.

### merge — the local merge queue

The fold-actor's per-branch land/defer status. Scoped to the active
workspace by default; `g` widens to every workspace. Keys: `a`/`A` add ·
`x` remove · `l` land · `r` retry · `D` drain. See [[merge-queue]].

### issues — the repo's tracker

Issues from the configured `[issues]` providers for this repo. Keys: `↵`
link/unlink the cursor issue to this worktree · `o` browser · `a` assign
to me · `b` branch a worktree from it · `D` dispatch an agent · `r`
refresh.

### problems — diagnostics

Compiler/linter/test diagnostics, collected from two sources: parsed
output of tasks you run in **jobs**, and language-server pushes for the
files you touch. Only the active worktree's diagnostics are shown — other
workspaces' problems never bleed in. `↵` opens the editor at file:line.

### jobs and tests

**jobs** runs the configured + auto-discovered tasks (`↵` run · `r`
re-run · `s` stop · `o` output). **tests** is the test-runner view (`r`
run · `R` all · `f` failed · `o` output · `b` bisect).

### symbols — outline & references

A document-symbol outline of the selected changed file (language server
first, tree-sitter fallback). Keys: `↵` go to definition · `r` find
references (Esc returns to the outline) · `h` hover docs · `o` back to
the outline.

## The system tab, section by section

### notifications

The inbox. In row mode: `x` marks the row read, `d` **deletes** it, `A`
shows read rows too, `/` searches (matching message, source, or worktree
path), `↵` widens the panel so the full text is readable.

`a` is the **clear-all**, and it covers more than the list: as well as
marking the notifications the inbox shows read (this repo's + host-global
rows by default — other repos' only under the `g` all-worktrees view) it
acknowledges the live "needs you" signals behind the `✋` chip — failing
CI, PR conflicts, changes requested — which are derived from the PR/CI
caches rather than from rows here. "This repo's" includes a row tagged to
the repo's own main checkout, which the list always showed but the clear
used to skip. `g` toggles the scope between this repo (the default) and
every worktree. Both are described under [[bars]].

An agent's `OSC 9` / `OSC 777` "I need you" is not a row here: it is live
state, shown as the sidebar dot and the `✋` chip and cleared when you
answer, unless you turn on `[notifications] agent_attention_inbox` for an
audit trail (one current row per session, not one per turn).

### logs

The live thegn log stream, scoped to this worktree + host-global lines by
default. `/` filters by text (Esc clears), `l` cycles the minimum level,
`y` copies the highlighted line and `Y` every visible line, `a` toggles
tail-follow, `g` widens to every worktree, and `E` exports the visible
lines to a file. What you see is exactly what copy/export operate on.
Requires logging to be on (`THEGN_LOG`, or `[log] file = true`).

### sandbox, hosts, environments

**sandbox** shows the focused worktree's sandbox state (`g` widens to
every container); `s` stops / `r` restarts the highlighted container
(the worktree's own one at the narrow widths) and `l` tails its logs in
a pane — see [[sandboxing]]. At full width the containers table sits
beside the activity timeline. **hosts** manages `[host.*]`
machines: `n` adds one, `p` provisions, `r` re-probes, `m` opens the
action menu (which holds removal), `c` grants install consent, `x`
forgets cached state. **environments** lists `[env.*]`: `↵` binds the
env to this worktree, `t` tests its token, `n` adds, `x` removes
(confirmed).

### share, forward, media, telemetry, keys

**share** and **forward** list the worktree's tunnels and port forwards
(`↵` copy · `o` browser · share's `x` stops) — see [[share-and-forward]].
**media** is the now-playing view ([[media]]). **telemetry** is the live
stats/loop-profiler view. **keys** is the generated cheatsheet of the
effective keymap.

### db and debug — reserved placeholders

Two system-tab section names are reserved stubs, not features: **db**
renders "no database detected" over a "db introspection not wired yet"
line, and **debug** renders "no session", an empty `BREAKPOINTS` list and
a "debugger integration not wired yet" line. Neither has keys or
behaviour behind it, and neither appears in the built-in accordion — they
stay out of the tab rotation until the database and debugger
integrations land, at which point they become real sections and this
page will document them as such.
