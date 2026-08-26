---
id: merge-queue
title: Merge queue
parent: workflows
order: 2
contexts: [panel:merge]
actions: [integrate, merge-drain, open-merge-queue, sweep-merged]
---

# Merge queue

A **local** merge queue: finished branches queue up, and a fold-actor
lands them on `main` one by one — fold, gate, advance the ref — without
ever checking `main` out. Enable it with `[merge_queue] enabled = true`.

## Queueing

Add the current worktree's branch from the [[panel]]'s _merge_ section,
the sidebar row menu, or `thegn merge add` in the pane. The queue is
persisted; entries survive restarts. The section shows the active
workspace's queue by default; `g` widens it to every workspace.

## Landing

- **Integrate** (palette, or the section's key) drains the queue once:
  each queued branch is folded into `main` and gated before the ref
  advances; conflicted branches stay queued.
- **Drain (agent autopilot)** hands conflicts to a coding agent to
  resolve, then continues.
- `thegn integrate` does the same from any shell. It prints the branches
  it is about to fold and asks before folding any of them; `--dry-run`
  prints that plan and stops, and `--yes` skips the prompt (required to
  fold non-interactively).

Only branches you **queued** are folded. `--all`, or `[merge_queue]
require_enqueue = false`, widens it to every _eligible_ worktree branch —
which means every clean branch not already on the target, including work
still in progress. That test cannot tell a finished branch from one you
are midway through, and a land is awkward to walk back even with the
grace period below. Prefer `thegn merge add`.

- A blocked branch is re-attempted by the next drain automatically. Once
  it has burned its `agent_max_attempts` the agent stops being
  dispatched for it — `thegn merge retry` (or the section's `r`) re-arms
  it after you've fixed something.

## Land strategy, signing, drivers

Every land advances the target ref only by an object-DB fold + gate +
compare-and-swap, defers the whole branch on any conflict, and is a no-op
for a branch already merged. Within that, `[merge_queue]` lets you shape it:

- **`land_strategy`** — `merge` (the default 2-parent merge commit),
  `squash` (one single-parent commit with the merged tree), or `rebase`
  (`linear`; the branch's own commits replayed one at a time, keeping
  their original author). `land_message` templates the merge/squash
  subject (`{branch}`, `{target}`, `{subjects}`).
- **`sign_commits`** — sign the fold/land commits thegn creates. Signing
  is always non-interactive: a locked agent, a missing key, or anything
  that would prompt stops the drain as an _infrastructure_ error with a
  reason — it never marks the branch `needs_human` and never wakes the
  fixing agent, because a signing fault is not the branch's fault.
  `thegn doctor` probes signing readiness when this is on.
- **`rerere`** — reuse recorded conflict resolutions across drains
  (shared `rr-cache`), so a conflict resolved once auto-resolves next
  time. A rerere-resolved merge still runs the gate before landing.

When a conflicted path is governed by a custom `.gitattributes
merge=<driver>` the object-DB fold cannot honor, that branch is folded
through a throwaway-worktree real `git merge` so the driver runs, then
the result is gated and landed normally. Clean folds pay none of this.

## After it lands

`on_landed` decides what happens to a worktree whose branch just landed.
The default is **`expire`**: the worktree is filed into the _Merged_
folder and kept there for `merged_ttl_secs` (7 days out of the box),
then swept away along with its branch.

The grace period exists because the two halves of a land are not equally
recoverable. The branch ref is the merge commit's second parent, so it
costs one command to recreate — but the worktree **directory** holds
gitignored state (`target/`, `.direnv`, env files) that exists nowhere
else. A week is how long you have to notice.

- **Sweep merged** (palette, or `thegn merge sweep`) collects everything
  already past its grace period, now. `--force` ignores the remaining
  time and clears the lot.
- The _merge_ section's **`c`** does the same for that repo: it clears the
  landed rows and sweeps their worktrees together. Under `expire` a landed
  row _is_ the grace-period clock, so clearing one without its worktree
  would strand the worktree in _Merged_ with nothing left to collect it.
- A merged worktree you have gone back to and **edited is never swept**,
  forced or not. Commit or discard the changes and it becomes eligible
  again.
- The sweep runs at startup and after each land. There is no timer, so
  something that comes due while thegn is closed is collected at the next
  launch rather than at the stroke of the deadline.

Set `merged_ttl_secs = 0` to keep merged worktrees indefinitely (the same
as `on_landed = "move"`), or `on_landed = "remove"` to delete immediately
with no grace period at all.

A one-shot **`thegn land`** (the blessed alternative to `git checkout main
&& git merge`) files the worktree into _Merged_ too, but always **leaves it
in place**: it never removes the worktree or deletes the branch, whatever
`on_landed` says, because it is routinely scripted from _inside_ the worktree
it lands. It also records no queue row, so a worktree it files is **not**
expiry-swept — only branches landed through the queue (a `landed` row) are
collected. Remove such a worktree yourself once you're done with it. (Under
`on_landed = "off"` a `thegn land` instead clears any stale _Merging_ /
_Needs attention_ membership, so a fold-actor land never strands it.)

The fold advances the ref without checking anything out, so any worktree
sitting **on** the target would be left with a stale working tree. Every
such checkout — the main one or a linked worktree — is fast-forwarded as
part of the land. One with real uncommitted work is left alone and
_named_, with the `git reset --keep` that syncs it: don't commit the
pending deletions `git status` shows there, they are the fold, not a
deletion.

## Across hosts

The queue is **anchored to the target repo** — the host where `main`
lives — because the fold happens inside that repo's object store. You can
still queue branches whose worktrees live on **other machines**: each
row's host is shown as an `@host` chip in the _merge_ section, and at
drain time the branch's tip is bundle-fetched into the target store
before it folds. A branch whose host is unreachable is **deferred** (with
the reason), never silently dropped, and retried on the next drain.

Run the drain **where the target repo lives**. If you invoke it from a
machine other than the target's host, thegn tells you which host to run
it on — the fold, gate, and ref-advance must be co-located with `main`.

## The fixing agent

Point the handoff at an agent either by naming one of your `[[agents]]`
entries — thegn supplies the headless flags for its provider — or by
writing the command yourself:

```toml
[merge_queue]
conflict_handoff = "agent"
agent = "claude"            # an [[agents]] name; thegn adds `-p …`
# agent_command = "..."     # ...or spell it out; this wins when both are set
```

An agent thegn doesn't recognize still works: the prompt is appended as
an argument. With neither key set the handoff quietly becomes "notify",
so a drain says so out loud when `agent` names nothing that exists.

**Write placeholders bare.** Values are shell-quoted for you, so
`-p {prompt}` is right and `-p "{prompt}"` hands the agent a prompt
wrapped in literal quote characters. `thegn config validate` catches it.

What the agent is _told_ is yours to change. Leave a prompt empty for
thegn's built-in instructions, or override one per blocker kind:

```toml
[merge_queue.prompts]
conflict = "Fix {branch} for {target}. Conflicts:\n{paths}"
# gate_failure gets {log} instead of {paths}
```

Placeholders are checked against the kind (`conflict` gets `{branch}`,
`{target}`, `{worktree}`, `{paths}`; `gate_failure` swaps `{paths}` for
`{log}`) — a typo is a config error, never a blank sent to the agent.
A repo can carry its own via `[workspace.<slug>.merge_queue.prompts]`,
merged key by key with your global ones.

If you replace a prompt, keep its rules: the agent must commit on the
branch and must **not** push or touch the target. thegn does the fold and
ref-advance itself, which is what keeps the object store coherent.

## The gate

The folded tip is checked out into a **bare** gate worktree — a fresh
checkout of the merge commit with no dependencies installed. That is the
point (it tests the union, which exists nowhere else), but it means
`gate_command` cannot rely on a project-local toolchain: no
`node_modules`, no virtualenv, no `.direnv`. Rust works out of the box
only because `cargo` is global.

Provision it with `gate_setup_command`, which runs first and is **not**
part of the verdict:

```toml
[merge_queue]
gate_setup_command = "pnpm install --frozen-lockfile"
gate_command = "pnpm test"
```

That split matters, because a gate can fail two different ways and only
one of them is about your branch:

- **`gate_failed`** — the gate ran and went red. A verdict about the
  code, and the one a fixing agent can act on.
- **`gate_error`** — the gate could not run at all (missing binary,
  setup failed, killed). An environment fact. It never wakes the agent,
  never triggers a bisect, and never blames a branch — but it is retried
  on the next drain, since the environment may be fixed by then.

## Driving it from an agent pane

thegn ships the queue's instructions to whatever coding agent you run in a
pane, so you don't hand-install anything. When the queue is enabled, each
worktree is seeded with three Claude-format assets:

- `.claude/skills/mq/SKILL.md` — `/mq`, the overview an agent finds on its
  own when you ask it to queue or check a branch.
- `.claude/commands/mq-add.md` — `/mq-add`, commit this worktree's branch
  and enqueue it.
- `.claude/commands/mq-drain.md` — `/mq-drain`, work the queue by hand:
  land what is clean, resolve the rest yourself, repeat. The
  agent-autopilot equivalent of `thegn merge drain`, for when you'd rather
  drive than configure `conflict_handoff`.

They are written at worktree-create time and back-filled at startup, and
each is added to the repo's `.git/info/exclude`, so they never show up as
untracked changes. Nothing is seeded while `[merge_queue] enabled = false`.
Edit them freely — they are rewritten on the next launch, so keep local
changes elsewhere.

## Watching

The status bar shows queue depth ([[bars]]); the _merge_ section lists
entries with their gate state. Failures surface as notifications.
