# Design — agent-driven merge-queue driver

## Where the agent works vs. where thegn lands

The invariant that makes the fold-actor safe is that `main` only ever advances
through a pure object-DB fold + atomic CAS, never a working-tree merge (that's
what `merge_guard` enforces against in-sandbox canonical merges). The driver
keeps that intact by splitting the work:

- **The agent** runs in the _branch's own linked worktree_ and only has to make
  the branch clean against the target: rebase onto the latest target, resolve
  conflicts, fix whatever the gate flagged, commit (no push). Rebasing a linked
  worktree is allowed — the guard only fires in the canonical checkout.
- **thegn** does the land: `attempt_land` re-reads the branch tip, folds it
  onto the current target with `fold::fold`, gates the result, and CAS-advances
  the ref + fast-forwards the main checkout (`resync_ff_checkout`).

So the agent never touches `main`; the coherence guarantee is unchanged.

## The per-branch loop (`drive_queue`)

```
for each queued branch (oldest first):
  loop:
    match attempt_land(cfg, repo_root, branch):
      Landed{commit}       -> status=landed;  next branch
      UpToDate             -> status=landed ("already merged"); next branch
      Ready{tip}           -> status=ready (auto_land off); next branch
      Conflict{paths} |
      GateFailed{log}      -> if handoff==Agent && agent_command && runs<max:
                                 status=agent_running; run_agent(...); runs+=1; retry
                               else:
                                 status = runs>0 ? needs_human : deferred/gate_failed
                                 next branch
```

`attempt_land` owns the CAS-retry (re-read tip, re-fold) so a target moving under
the driver is handled exactly like `run_fold`. The re-attempt after the agent is
the sole arbiter of whether the fix worked — the agent's exit code is advisory.

## Headless agent invocation (`run_agent`)

`agent_command` is a shell template; `{prompt}`/`{branch}`/`{target}` are
substituted with `util::sh_quote` (single-quoted, so a prompt full of quotes and
newlines is one safe word — templates use bare `{prompt}`, not `"{prompt}"`).
The command runs via `$SHELL -lc` (login shell, so an npm-global `claude` is on
PATH with the user's credentials, like an interactive agent pane), cwd = the
worktree, stdio captured (never written to the compositor terminal), git env
scrubbed (the agent's `git` targets its cwd), under a `agent_timeout_secs`
watchdog that kills the process group. The task prompt states the rules: work
only in this worktree, commit, do not push, do not touch the target.

## Statuses

`merge_queue.status` is free-text TEXT — new values `agent_running`, `ready`,
`needs_human` need no schema migration. The panel section and CLI render them;
`ready` (auto_land off) is landed later by `merge land`.

## Why the CLI is the surface (for now)

`run.rs` (22138/22139) and `keymap.rs` (3028/3028) sit at their god-file ratchet
ceilings, so a new `Action` variant + loop wiring + `auto_drain`-on-`AgentEnd`
can't be added without first extracting an existing block into a sibling module.
The driver is written off-loop and callable from a future `spawn_drive`; until
that extraction lands, `thegn merge add`/`drain` is the (fully programmatic)
entry point.
