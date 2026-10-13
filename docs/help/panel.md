---
id: panel
title: Right panel
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

# Right panel

The right panel tracks the focused worktree. `Alt-.` (or `Ctrl-→` from the
rightmost pane) focuses it; `Ctrl-Alt-p` hides it. It is a tabbed
accordion — four tabs, one open section at a time:

- **git** — changes (diff), commits, branches, stash, files
- **work** — mine (your unified work feed), across (cross-worktree
  attention), the branch's PR + the repo's open PRs, CI runs, the merge
  queue, the PR queue, issues, problems, jobs, tests, symbols
- **system** — notifications, logs, sandbox, hosts, environments, shares,
  port forwards, telemetry, media, usage, keys — plus two **reserved**
  stubs, `debug` and `db` (see below)
- **help** — this documentation, docked (the twin of the `F1` overlay)

## Keys

- `Tab` / `Shift-Tab` — cycle tabs; `1`–`9`, `0` jump to the tab's first ten sections
- `j k` / `↑↓` — move through rows; Help, Telemetry, and Keys scroll their
  body. At a section's edge, `j k` flow into the next section, stopping at
  the tab's first/last section.
- `↵` — act on the cursor row (open, jump, link — per section below);
  `Esc` steps back out of the rows
- `e` — cycle the width: normal → half → full-screen; `E` shrinks the width,
  except in git **commits**, where it edits/rewords.
- `F1` — open the full help overlay; git-family sections also show their
  own gitui cheatsheet on `?`

The status bar shows each section's keys. [[keybindings]] lists the complete
keymap, including section tables under `Panel · <section>` headings.

`[panel] sections` reorders the accordion or hides sections you never
use; the built-in order is the default.

## What each section shows

**git**

| Section    | Shows                          |
| ---------- | ------------------------------ |
| `changes`  | the working diff, hunk by hunk |
| `commits`  | branch history                 |
| `branches` | local branches                 |
| `stash`    | stash entries                  |
| `files`    | the worktree tree              |

**work**

| Section    | Shows                                                                 |
| ---------- | --------------------------------------------------------------------- |
| `mine`     | one feed of everything assigned to you: issues, review requests, PRs  |
| `across`   | failing CI for the active project (`a` widens), grouped by worktree   |
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

| Section         | Shows                                                                     |
| --------------- | ------------------------------------------------------------------------- |
| `notifications` | the notification list (see below)                                         |
| `logs`          | thegn's own log stream                                                    |
| `sandbox`       | live sandbox state for this worktree — see [[sandboxing]]                 |
| `hosts`         | configured `[host.*]` machines and their state                            |
| `environments`  | configured `[env.<name>]` environments                                    |
| `share`         | ports this worktree exposes — see [[share-and-forward]]                   |
| `forward`       | auto port forwards to host loopback                                       |
| `telemetry`     | live frame/loop counters for the running UI                               |
| `media`         | now-playing and transport — see [[media]]                                 |
| `usage`         | AI-account rate-limit usage — see [[ai-usage]]                            |
| `keys`          | first chord per action plus panel keys; [[keybindings]] has the full list |

`hosts` and `environments` are **dev-channel only**; see
[[release-channels]].

The merge queue drives the local fold-actor, and the PR queue shepherds
pull requests on the forge — see [[merge-queue]] and [[pr-queue]] for
those workflows.

## Width: memory, DWIM, config, drag

- **Per-section memory** — each section remembers the width you last gave
  it (persisted); hopping sections restores it, so a full-screen Logs and a
  narrow Changes coexist.
- **Auto-widen (DWIM)** — an action that needs a wider view (for example,
  `s` squash at resting width) widens the panel and performs the action in
  one press instead of printing a "widen first" note.
- **`[panel] width` / `half_ratio`** — the resting column count and the
  half-screen fraction are configurable (see the configuration reference).
- **Drag** — at the resting width, drag the panel's left separator to resize;
  the grab takes the divider **or the pane edge beside it** (the two read as
  one boundary), the divider keeps its grab offset, and the width persists on
  release and becomes the new resting width. A press that never moves changes
  nothing, and `Esc` cancels a resize, restoring the width you started with.

## The git tab, section by section

### changes — the working diff

The working diff, hunk by hunk, for the focused worktree. `↵` on a file
inlines its hunks (binary and mode-only changes say so instead of a
diff); `space` stages; `e` widens to the full-screen git frame, where
`↵` drills into **staging** (line-level stage/unstage).

{{keys:changes}}

Config: the
`[git] structural_diff` setting renders the read-only diff surfaces
through difftastic. See [[git-and-diffs]].

### commits — branch history

The branch's log; pick a commit to operate on. `E` edits (interactive
rebase stopping at the commit); bare `e` is always the panel width cycle.

{{keys:commits}}

See [[git-and-diffs]].

### branches — local branches

Local branches: check out, create, delete; open-PR badges ride each row.

{{keys:branches}}

See [[git-and-diffs]].

### stash — stash list

Stash list: apply, pop, drop; `↵` shows the stash diff. At Full width,
PgUp/PgDn and Ctrl-d/Ctrl-u scroll it. See [[git-and-diffs]].

{{keys:stash}}

### files — the worktree tree

The worktree's file tree; `/` filters it (directories stay while any
descendant matches; Esc clears), `↵` previews a file inline, `o` pages
it in bat, `O` opens your editor, `y` reveals it in yazi. See
[[git-and-diffs]].

{{keys:files}}

Inside the git-family sections (changes/commits/branches/stash — not
files), `?` shows that section's own key cheatsheet.

## The work tab, section by section

### mine — your unified work feed

Everything waiting on _you_, aggregated across tools and grouped as
**Review requested** · **Needs attention** · **Assigned to me**: issues
assigned to you (every configured `[issues]` provider), PRs where your
review is requested or that you authored (via `gh search`), and
high-priority notifications. Scoped to the active repo by default; `a`
widens to every repo. A `◈` marks a row already linked to a worktree.

{{keys:mine}}

If the repo has no `origin` remote the repo-scoped PR search cannot run;
the section says so rather than silently searching every repo.

### across — cross-worktree attention

A read-mostly stream of things needing attention in your _other_
worktrees — currently each worktree's failing CI (latest run per
workflow), grouped by worktree. Scoped to the active project by
default; `a` widens to every project. `↵` jumps to that worktree's tab.

{{keys:across}}

### pr — the branch's PR, and the repo's open PRs

The top shows the current branch's PR: state, CI check rollup, review
decision, and unresolved review threads. Below it, **OPEN PRS** lists the
repo's other open PRs (`◈` = that branch already has a local worktree),
so the section answers "what's open on this repo", not only "what's open
on this branch".

`M` asks for a merge method (squash by default) and lists blockers. `c`
comments when a PR exists and asks before creating one otherwise; `r` needs
a PR. Enter opens the PR view. At Full width it shows a two-column page:
header and checks beside threads and OPEN PRS.

{{keys:pr}}

See [[review-a-pr]] for the full workflow.

### ci — pipeline state and cached logs

One row per workflow, judged by its most recent run — the same set the
header counts, so the `✓/✗` numbers always match the rows. Fan-out
workflows (several sibling runs from one trigger) are represented by a
failing sibling when there is one. History is behind the drill-in.

{{keys:ci}}

`↵` and `v` open the run's detail popup. `f` is available inside that popup,
not as a panel key. The drill reads bounded, redacted
job-log tails from the CI cache first and labels their source, age, truncation,
and redaction state while a missing or stale entry is fetched off-loop. In
`suggest` mode, the same `f` action is available from the actionable notification
when its evidence-ready message appears. The cache policy is controlled by
`[ci].log_cache_runs`, `log_tail_lines`, and `log_max_bytes`; `[ci.autofix]` is
off by default and can suggest or safely hand a verified failure to the
existing PR-queue agent. The action rechecks PR context, head, agent policy,
and attempt budget off-loop, and fails closed if any check no longer holds.

### merge — the local merge queue

The fold-actor's per-branch land/defer status. Scoped to the active
project by default; `g` widens to every project.

{{keys:merge}}

See [[merge-queue]].

### prq — the PR queue

The queued pull requests on the forge and what blocks each one — the
merge queue's counterpart for a **shared** repo. Off by default; turn
it on with `[pr_queue] enabled = true`.

{{keys:prq}}

The CLI equivalents are `thegn pr queue …`. See [[pr-queue]].

Additional keys: `h` invokes the configured handoff for a selected queued PR.

### issues — the repo's tracker

Issues from the configured `[issues]` providers for this repo.

{{keys:issues}}

### problems — diagnostics

Compiler/linter/test diagnostics, collected from two sources: parsed
output of tasks you run in **jobs**, and language-server pushes for the
files you touch. Only the active worktree's diagnostics are shown — other
projects' problems never bleed in. `↵` opens the editor at file:line.

{{keys:problems}}

### jobs and tests

**jobs** runs configured and auto-discovered tasks, each in its own slot;
tests run in a separate slot. `↵` or `r` on a running job asks you to press
again to restart it. `s` stops only the selected job.

{{keys:jobs}}

**tests** is the test-runner view.

{{keys:tests}}

### symbols — outline & references

A document-symbol outline of the selected changed file (language server
first, tree-sitter fallback).

{{keys:symbols}}

## The system tab, section by section

### notifications

The inbox. In row mode, `x` marks the row read and `d` **deletes** it. `/` searches
every unread row; a committed filter stays visible. The header counts alert,
unread, low-priority, and shown rows. The inbox retains every unread row and
the newest 50 read rows.

{{keys:notifications}}

`a` clears the inbox and acknowledges live "needs you" signals. It asks
for confirmation and ignores an active filter. `g` toggles scope between
this repo and every worktree. The popup's `a` does not ask for confirmation;
see [[bars]].

Additional keys: `A` shows read rows · `/` searches the inbox.

An agent's `OSC 9` / `OSC 777` "I need you" is not a row here: it is live
state, shown as the sidebar dot and the `✋` chip and cleared when you
answer, unless you turn on `[notifications] agent_attention_inbox` for an
audit trail (one current row per session, not one per turn).

### logs

The live thegn log stream, scoped to this worktree + host-global lines by
default. `/` filters by text (Esc clears), `l` cycles the minimum level,
`y` copies the highlighted line and `Y` every visible line, `a` toggles
tail-follow, `g` widens to every worktree, and `w` exports visible lines
to a file. What you see is exactly what copy/export operate on.
Requires logging to be on (`THEGN_LOG`, or `[log] file = true`).

{{keys:logs}}

### sandbox, hosts, environments

**sandbox** shows the focused worktree's sandbox state. Containers thegn
did not create are listed read-only: `s` and `r` are refused with a status,
while `l` still opens their logs. `s` and `r` on thegn's own containers ask
first. `g` scopes the full table too; Half-width `g` rows are selectable.
When sandboxing is off, a one-line banner says so. See [[sandboxing]]. **hosts** manages `[host.*]`
machines: `n` adds one, `p` provisions, `r` re-probes, `m` opens the
action menu (which holds removal), `c` grants install consent, `x`
forgets cached state. **environments** lists `[env.*]`: `↵` (re)binds the
env to this worktree, `t` tests provider env tokens only, `n` adds, `x`
removes (confirmed).
The environment currently bound to the worktree carries a visible bound marker.

{{keys:sandbox}}

{{keys:hosts}}

{{keys:environments}}

### share, forward, media, telemetry, keys

**share** and **forward** list the worktree's tunnels and port forwards
(`↵` copy · `o` browser · share's `x` stops) — see [[share-and-forward]].
**media** is the docked now-playing view ([[media]]): `Alt-m` and the media
badge focus the same System section. Its Normal/Half/Full widths progressively
show track, controls, and queue/artwork.

{{keys:media}}

Additional keys: `+`/`-` adjust volume · `[`/`]` move chapters · `f`
toggles fullscreen.

{{keys:share}}

{{keys:forward}}

**telemetry** scrolls and shows live stats/loop-profiler data; DISK shows
one row per device with used percent.

{{keys:telemetry}}

**keys** is a cheatsheet with the first chord per action plus panel keys;
[[keybindings]] has the complete list.

{{keys:keys}}

### usage — AI-account rate-limit usage

The docked twin of the `Alt-u` AI-usage overlay (see [[ai-usage]]):
accounts listed worst-first, the one nearest a limit at the top. The
width ladder applies — at the resting width one row per account showing
its worst limit, at half width every limit, at full width the identity
facts line plus a legend. `r` re-gathers now instead of waiting out the
poll. On by default; `[usage] enabled = false` turns the feature off
and hides this section. The `[usage] warn_percent` / `crit_percent`
thresholds colour it with the same gauge and overlay.

{{keys:usage}}

## The help tab

**help** shows this documentation in the docked panel. `j`/`k`, `↑`/`↓`,
PgUp/PgDn, `g`/`G`, and the wheel scroll the body; `Tab` switches panel
tabs. Use `F1` to open the full help overlay. No config; the content itself
is described in [[help]].
