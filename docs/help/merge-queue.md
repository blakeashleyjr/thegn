---
id: merge-queue
title: Merge queue
parent: workflows
order: 2
contexts: [panel:merge]
actions: [integrate, merge-drain, open-merge-queue]
---

# Merge queue

A **local** merge queue: finished branches queue up, and a fold-actor
lands them on `main` one by one — fold, gate, advance the ref — without
ever checking `main` out. Enable it with `[merge_queue] enabled = true`.

## Queueing

Add the current worktree's branch from the [[panel]]'s _merge_ section,
the sidebar row menu, or `thegn merge add` in the pane. The queue is
persisted; entries survive restarts.

## Landing

- **Integrate** (palette, or the section's key) drains the queue once:
  each clean branch is folded into `main` and gated before the ref
  advances; conflicted branches stay queued.
- **Drain (agent autopilot)** hands conflicts to a coding agent to
  resolve, then continues.
- `thegn integrate` does the same from any shell.
- A blocked branch is re-attempted by the next drain automatically. Once
  it has burned its `agent_max_attempts` the agent stops being
  dispatched for it — `thegn merge retry` (or the section's `r`) re-arms
  it after you've fixed something.

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

## Watching

The masthead shows queue depth ([[bars]]); the _merge_ section lists
entries with their gate state. Failures surface as notifications.
