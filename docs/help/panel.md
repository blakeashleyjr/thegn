---
id: panel
title: Right panel
order: 5
contexts:
  [
    zone:panel,
    panel:mine,
    panel:across,
    panel:ci,
    panel:issues,
    panel:problems,
    panel:jobs,
    panel:tests,
    panel:symbols,
    panel:notifications,
    panel:logs,
    panel:hosts,
    panel:telemetry,
    panel:keys#share-forward-media-telemetry-keys,
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
  port forwards, telemetry, keys, media, usage
- **help** — this documentation, docked (a scrollable reader alongside the
  `F1` overlay)

## Keys

- `Tab` / `Shift-Tab` — cycle tabs; `1`–`9`, `0` jump to the tab's first ten
  sections; later sections have no number
- `j k` / `↑↓` — move through rows; Help, Telemetry, and Keys scroll their
  body. At a section's edge, `j k` flow into the next section, stopping at
  the tab's first/last section.
- `↵` — act on the cursor row (open, jump, link — per section below);
  `Esc` steps back out of the rows
- `e` — cycle the width: normal → half → full-screen; `E` shrinks the width,
  except in git **commits**, where it edits/rewords.
- `F1` — open the full help overlay; git-family sections also show their
  own gitui cheatsheet on `?`
- Long lists show `↑N above` and `+N more` at every width. At Full width,
  the list sits beside a detail column pinned to the cursor row.

The status bar shows each section's keys. [[keybindings]] lists the complete
keymap, including section tables under `Panel · <section>` headings.

`[panel] sections` reorders the accordion or hides sections you never use;
the built-in order is the default. Naming a gated section there shows its
off banner when its feature is unavailable.

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

| Section    | Shows                                                                |
| ---------- | -------------------------------------------------------------------- |
| `mine`     | one feed of everything assigned to you: issues, review requests, PRs |
| `across`   | failing CI for the active project (`a` widens), grouped by worktree  |
| `pr`       | PR state, CI check rollup, review decision for this branch           |
| `ci`       | run history and per-run state across providers                       |
| `merge`    | the local merge queue — per-branch land/defer status                 |
| `prq`      | PR queue (hidden until `[pr_queue] enabled = true`)                  |
| `issues`   | tracker issues                                                       |
| `problems` | compiler, linter, and test diagnostics                               |
| `jobs`     | configured shell jobs (build, test, run)                             |
| `tests`    | test results and the pass/fail rollup                                |
| `symbols`  | the LSP / tree-sitter outline for the file you edit or pick          |

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
| `keys`          | first chord per action plus panel keys; [[keybindings]] has the full list |
| `media`         | now-playing and transport — see [[media]]                                 |
| `usage`         | AI-account rate-limit usage — see [[ai-usage]]                            |

On the stable channel, `hosts` and `environments` are hidden unless named in
`[panel] sections`; when named, each shows a one-line off banner and has no
live keys. They are available on the dev channel; see [[release-channels]].

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

The working diff, hunk by hunk, for the focused worktree. Binary and
mode-only changes say so instead of showing hunks. If git cannot produce a
file's diff, the preview says `diff unavailable (git failed)`. `↵` on a file
opens line-level **staging** at every width; at Normal width it widens to Half,
and Esc retracts that drill. At Half, the inline preview follows the cursor.
`space` stages, and `e` cycles panel width. Untracked files preview as all
additions and are intent-added (`add -N`) only on the first stage. In staging,
line discard is refused in the STAGED pane (unstage with `space`) and on an
untracked file (`d` on its Changes row deletes the file).

{{keys:changes}}

Config: the
`[git] structural_diff` setting renders the read-only diff surfaces
through difftastic. See [[git-and-diffs]].

### commits — branch history

At Half and Full width, the detail region follows the cursor and shows the
commit as a read-only `git show` view. `↵` opens that commit's files with the
selected file's diff shown read-only. Mark lines with `space` to start a
custom patch; `W` diffs the selection against the mark. `E` edits
(interactive rebase stopping at the commit); bare `e` is always the panel
width cycle. `B` marks the rebase base for `i` and `r`; see
[[git-and-diffs]] for the base-mark behavior.

{{keys:commits}}

See [[git-and-diffs]].

### branches — local branches

Local branches: check out, create, delete; open-PR badges ride each row.
At Normal width, `↵` widens to Half; the selected branch's recent commits
appear at Half and Full width. `p` pulls and `P` pushes HEAD only, refusing
when another row is selected; `n` opens the new-branch name input. Menu
pull and push also operate on HEAD only. Before the first load, the list says
"loading…"; a failed read names the error instead of appearing empty.

{{keys:branches}}

See [[git-and-diffs]].

### stash — stash list

At Normal width, `↵` widens to Half. The stash diff appears and scrolls at
Half and Full width, including untracked files. The diff follows the stash
entry by its object ID. Apply, pop, and drop remain available from the stash
list. See [[git-and-diffs]].
Before the first load, the list says "loading…"; a failed read names the
error instead of appearing empty.

{{keys:stash}}

Inside the git-family sections (changes/commits/branches/stash — not
files), `?` shows that section's own key cheatsheet.

### files — the worktree tree

The worktree's file tree; `/` filters it (directories stay while any
descendant matches; Esc clears), `↵` previews a file inline, `o` pages
it in bat, `O` opens your editor, `y` reveals it in yazi. An unknown binary
previews as a hex dump of its first 4096 bytes. See
[[git-and-diffs]].

{{keys:files}}

## The work tab, section by section

### mine — your unified work feed

Everything waiting on _you_, aggregated across tools and grouped as
**Review requested** · **Needs attention** · **Assigned to me**: issues
assigned to you (every configured `[issues]` provider), PRs where your
review is requested or that you authored (via `gh search`), and
high-priority notifications. Scoped to the active repo by default; `a`
widens to every repo. A `◈` marks a row already linked to a worktree.
Enter opens the linked worktree when present, otherwise the PR view in-app
for a PR in this repo, otherwise the browser. `b` branches only from issue
rows and refuses when there is no local checkout, including in all-repos
scope.

{{keys:mine}}

If the repo has no `origin` remote the repo-scoped PR search cannot run;
the section says so rather than silently searching every repo.

### across — cross-worktree attention

A read-mostly stream of things needing attention in your _other_
worktrees — currently each worktree's failing CI (latest run per
workflow), grouped by worktree. Scoped to the active project by
default; `a` widens to every project. `↵` jumps to that worktree's tab,
or opens the registered worktree if it has no open tab.

{{keys:across}}

### pr — the branch's PR, and the repo's open PRs

The top shows the current branch's PR: state, CI check rollup, review
decision, and unresolved review threads. Below it, **OPEN PRS** lists the
repo's other open PRs (`◈` = that branch already has a local worktree),
so the section answers "what's open on this repo", not only "what's open
on this branch".

OPEN PRS rows are selectable. Enter on one jumps to its worktree when
available, or opens a read-only view of that PR; write actions are refused
there. `w` creates or jumps to a worktree on that PR's head branch. `M`
asks for a merge method (squash by default), lists blockers, and pins the
merge to the PR head shown in the panel. If the PR moved, nothing is merged
and the status says so; a forge that cannot pin the head refuses. `c`
comments when a PR exists and asks before creating one otherwise; `r` needs
a PR. Enter on the branch PR opens its view. At Full width it shows a
two-column page: header and checks beside threads and OPEN PRS.

{{keys:pr}}

See [[review-a-pr]] for the full workflow.

### ci — pipeline state and cached logs

Every run is listed. The header counts the latest run per workflow, so the
`✓/✗` numbers match the current-state summary. Fan-out workflows (several
sibling runs from one trigger) use a failing sibling for the summary when
there is one. At Half and Full width, the detail follows the highlighted run.

{{keys:ci}}

`v` (or Enter) opens the run's detail popup. The **f** action is available
inside that popup, not as a panel key. The drill reads bounded, redacted
job-log windows (newest lines, or a window around the first failure) from the CI
cache first and labels their source, age, truncation,
and redaction state while a missing or stale entry is fetched off-loop. In
`suggest` mode, the same **f** action is available from the actionable
notification when its evidence-ready message appears. The cache policy is controlled by
`[ci].log_cache_runs`, `log_tail_lines`, and `log_max_bytes`; `[ci.autofix]` is
off by default and can suggest or safely hand a verified failure to the
existing PR-queue agent. The action rechecks PR context, head, agent policy,
and attempt budget off-loop, and fails closed if any check no longer holds.
In the section and in its popup, **r** reruns failed jobs, **R** reruns all
jobs, and **c** cancels after a second press. On GitLab, **r** retries failed
and canceled jobs together, while **R** is refused because GitLab has no full rerun.

The daemon serves a cached CI run list only while it is younger than
`[ci] ttl_secs` and recorded for the worktree's current branch; otherwise it
asks the provider. When the provider fails or returns an incomplete run list,
the daemon keeps its cached list (an incomplete list never replaces it) and
serves the last good list recorded for the same branch and provider as
`source: "stale-cache"` with a `stale_reason`; with no such list, the error is
returned. If a run's job detail cannot be fetched, the drill says "job detail
unavailable", keeps the cached run, and refuses **f**. Cached job logs written
under an older redaction policy are redacted and bounded into a window again
when read. When no CI provider resolves for the worktree, `ci.runs` and a
cache-missing `ci.logs` request refuse with `failed_precondition` (HTTP 412)
and a message starting `no-provider:`. `ci.runs` never serves a cached list
for a worktree with no provider.

Cached run lists are shown when their recorded provider context matches the
worktree's current context. A different or unknown cached context shows
"refresh pending (g)" until refresh; when local origin context is unavailable,
including for remote worktrees, the cached display is kept. CI autofix gives
the PR-queue agent a redacted log excerpt starting 20 lines before the first
recognised failure, or the newest lines if none is found; omitted lines are
marked.
Sidebar attention follows the same provider-context rule: a cached CI
failure from another context does not mark a worktree for attention, while an
unknown current context keeps the cached failure.
It also skips runs cached for another branch, or with no recorded branch, in
the sidebar and **across**; when the current branch is unknown, cached runs
still count.

#### CI log retention and privacy

The CI cache keeps logs for the newest `log_cache_runs` terminal runs per
worktree (default 10). Each job is limited by `log_tail_lines` (default 2000)
and `log_max_bytes` (default 1 MiB), with hard caps of 100,000 lines and
4 MiB. Logs are stored only in the local state database; thegn itself never
uploads them, but redacted excerpts are passed to the agent command you
configure for the PR queue and CI autofix. They are purged when provider
context changes. Redaction is pattern-based and best-effort, so treat cached
logs as sensitive. Set `log_cache_runs = 0` to disable persistence.

Thegn does not run workflows locally. To run them locally, add a tool such as
`act` and launch it from the tool picker:

```toml
[[tools]]
name = "act"
command = "act"
```

### merge — the local merge queue

The fold-actor's per-branch land/defer status. Scoped to the active
project by default; `g` widens to every project. `c` sweeps landed rows
after a second press. Enter widens to the Full detail.

{{keys:merge}}

See [[merge-queue]].

### prq — the PR queue

The queued pull requests on the forge and what blocks each one — the
merge queue's counterpart for a **shared** repo. Hidden until
`[pr_queue] enabled = true`; listing it in `[panel] sections` while disabled
shows a one-line off banner with no live keys.
Enter widens to the Full detail.

{{keys:prq}}

The CLI equivalents are `thegn pr queue …`. See [[pr-queue]].

`c` clears the queue after a second press. The `h` key invokes the configured
handoff for a selected queued PR.

### issues — the repo's tracker

Issues from the configured `[issues]` providers for this repo. Before the
first load, the list says "loading…"; a failed read names the error instead
of appearing empty. A Busy tracker refresh is retried.

When issue numbers are shared across tracker accounts, each row shows its
account in parentheses, including in My Work and the palette. If a tracker
listing stops early, an amber `partial results: <accounts> — ...` line names
the accounts and says whether the last complete list, partial list or cached
rows are shown.
Notifications for a shared issue number name the account, and overdue notices
for two accounts stay separate. With
`[issues] move_on_merge` enabled, a linked issue that cannot move to Done when
its PR merges raises an `automation_failed` inbox notification with the error
redacted; the same issue and error are reported once.

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
again to restart it. `s` stops only the selected job, which is labelled
`stopped`. A job killed after `[limits] test_timeout_secs` is labelled
`timed out after Ns`. While a job is running, its Half and Full detail shows
a `live output` tail of up to the last eight lines, bounded to 16 KiB. When a
finished job exceeded the capture limit, its record keeps the start, a
`[output truncated: N bytes not shown; the end follows]` marker, and the last
16 KiB, so the detail, output pager, and Problems show the real end. Test runs
keep the start only.

{{keys:jobs}}

**tests** is the test-runner view.

`f` reruns every failing test; `R` reruns the whole suite; `O` opens the
last run's output; `/` filters results (Enter commits the filter; Esc clears
it while editing); `u` rediscovers tests. The view also offers file and
package selection, debug launch, editor opening, and output peeking.

{{keys:tests}}

### symbols — outline & references

The outline targets the focused editor pane's file when it was opened from
thegn, otherwise a changed file you pick with `f`, otherwise the file
picker. Language server first, tree-sitter fallback. `r` opens references;
Esc returns to the outline. With no language server, references say so;
`h` reports when hover is off. A language server for a worktree you are not
in that sits unused for 30 minutes is stopped and restarts on next use. When
the worktree's language server is overloaded, outline and hover say
`language server busy` on the status line and references say so in the view;
try again. Esc out of references cancels a references lookup still in flight;
switching worktree cancels outline, references and hover lookups.
A language-server binary that is world-writable, or owned by a user other than
you or root, is refused on Unix. For languages with a tree-sitter grammar, the
outline falls back to the tree-sitter parse when the server cannot start.

{{keys:symbols}}

## The system tab, section by section

### notifications

The inbox. In row mode, `x` marks the row read and `d` **deletes** it. **/** searches
every unread row; a committed filter stays visible. The header counts alert,
unread, low-priority, and shown rows. The inbox retains every unread row and
the newest 50 read rows. Read rows older than 30 days are deleted from the
state database; unread rows are never pruned. At Half width the highlighted
row shows its whole message; Full width puts it in the detail column.
At the next background maintenance round, read rows for a removed worktree
are deleted regardless of age; unread rows are never pruned.

{{keys:notifications}}

Enter jumps to the row's worktree or section and marks it read; `space`
expands the whole message in place. **o** opens an HTTP(S) source link.
`a` clears the inbox and acknowledges live "needs you" signals. It asks
for confirmation. With a non-empty filter, a menu offers "only the N
filtered" first and "all in scope" second; with no filter it stays a plain
confirm. `g` toggles scope between
this repo and every worktree. The popup's `a` also asks for confirmation;
see [[bars]].

Additional keys: **A** shows read rows · **/** searches the inbox.

An agent's `OSC 9` / `OSC 777` "I need you" is not a row here: it is live
state, shown as the sidebar dot and the `✋` chip and cleared when you
answer, unless you turn on `[notifications] agent_attention_inbox` for an
audit trail (one current row per session, not one per turn).

### logs

The live thegn log stream, scoped to this worktree + host-global lines by
default. `/` filters by text (Esc clears), `l` cycles the minimum level,
`y` copies the highlighted line and **Y** every visible line, `a` toggles
tail-follow, `g` widens to every worktree, and `w` exports visible lines
to a file. Moving up in the rows pauses tail-follow; `a` resumes it. Rows
show `HH:MM:SS` below Full width, and Half adds the target and worktree.
Copies go to the system clipboard and via OSC 52; a failed system copy is
reported. The summary counts the live stream.
Requires logging to be on (`THEGN_LOG`, or `[log] file = true`).

{{keys:logs}}

### sandbox, hosts, environments

**sandbox** shows the focused worktree's sandbox state. Containers not
proven to belong to this profile are listed read-only: ownership requires
this profile's expected name (or its recognized `-tgvpn`/`-tgagent`
companion) for a known worktree, on that worktree's launch backend.
A user's `thegn-foo`, another profile's container, or the same name in the
other Podman store is foreign; a new container may appear foreign until the
next refresh. `s` and `r` are refused with a status for foreign containers,
while `l` still opens their logs. Actions on owned containers ask first.
`g` scopes the full table too; Half-width `g` rows are selectable.
When sandboxing is off, a one-line banner says so. See [[sandboxing]]. **hosts** manages `[host.*]`
machines: `n` adds one, `p` provisions, `r` re-probes, `m` opens the
action menu (which holds removal), `c` grants install consent, `x`
forgets cached state. **environments** lists `[env.*]`: `↵` (re)binds the
env to this worktree. If placement changes, it asks first; new panes move
to the new placement while running panes stay where they are. A same-placement
bind is immediate. `t` tests provider env tokens only, `n` adds, `x` removes
(confirmed).
The environment currently bound to the worktree carries a visible bound marker.
At Half and Full width, the highlighted environment's detail shows its
placement, host, target, data, sandbox, failover, and provider token variable
with a present or missing status. It never shows the token value.

At Full width, hosts shows the host list beside the highlighted host's
details, including its SSH/reach target.

{{keys:sandbox}}

{{keys:hosts}}

{{keys:environments}}

### share, forward, media, telemetry, keys

**share** and **forward** list the worktree's tunnels and port forwards
(`↵` copy · `o` browser · share's `x` stops). In **share**, `n` starts a
share; in **forward**, `n` watches a port and `x` removes a forward — see
[[share-and-forward]].
**media** is the docked now-playing view ([[media]]): `Alt-m` and the media
badge focus the same System section. Its Normal/Half/Full widths progressively
show track, controls, and queue/artwork.

{{keys:media}}

Additional keys: **+**/**-** adjust volume · **[**/**]** move chapters · **f**
toggles fullscreen.

{{keys:share}}

{{keys:forward}}

**telemetry** scrolls and shows live stats/loop-profiler data; DISK shows
one row per device with used percent.

{{keys:telemetry}}

**keys** is a cheatsheet with the first chord per action plus panel keys;
rows are grouped by section. `/` filters, and Enter reports where an action
is bound. [[keybindings]] has the complete list.

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
PgUp/PgDn, `g`/`G`, and the wheel scroll the body. `↵` or `F1` opens the
overlay at the same page and position; the overlay's `o` docks that page at
the same position. The docked page and position persist across restarts.
`Tab` switches panel tabs. No config; the content itself is described in
[[help]].
