# Design — worktree lifecycle hooks

## Event model and ordering

The load-bearing ordering decision is where hooks sit relative to env
provisioning. The create pipeline (today: wizard worker, off-loop) becomes:

```text
pre_create (host, BLOCKING)            cwd = repo root
  └─ git worktree add
       └─ DB register
            └─ built-in provisioning: prepare-alias entries, direnv warm
               (bounded-sync), devshell/mise resolve kick
                 └─ post_create (host)  cwd = worktree
                      ├─ default: parallel with first-pane spawn
                      └─ wait=true: first pane waits for completion
```

`post_create` runs **after** built-in provisioning so a hook like
`pnpm install` sees the direnv/devshell warmth the built-ins provide, and so
the legacy `prepare` list (which today runs at exactly that point) can fold in
as the head of `post_create` without observable reordering.

Destroy (user-invoked or unattended):

```text
session_end (if the worktree had a live session)
  └─ pre_destroy (host)                cwd = worktree
       ├─ user-invoked + failure → BLOCK, offer force
       └─ unattended (merge_lifecycle reclaim) + failure → warn, continue
            └─ git worktree remove
                 └─ post_destroy (host) cwd = repo root
```

Session hooks map to the compositor's real boundaries: `session_start` fires
when a worktree's first pane of a UI session spawns (not per pane — that is
`init_script`'s job), `session_end` when its last pane exits or the tab
closes. Both warn-only: a session must never fail to open or close over a
hook.

Within an event, execution is sequential in scope order — global →
`[workspace.<slug>]` → repo — and declaration order within a scope. Scopes
concatenate (accumulate), they do not override: teardown declared globally
must run even when a repo adds its own.

## Failure semantics (block vs warn)

| Event                           | Default                    | Rationale                                                                                      |
| ------------------------------- | -------------------------- | ---------------------------------------------------------------------------------------------- |
| `pre_create`                    | **block**                  | nothing exists yet; aborting is free and honest                                                |
| `post_create`                   | warn                       | the worktree is real; destroying it over a hook is worse                                       |
| `pre_destroy` (user-invoked)    | **block + force override** | the hook exists to protect state (running server, unreleased port); force is one keypress away |
| `pre_destroy` (unattended)      | warn                       | a dead script must not wedge merge-queue reclaim                                               |
| `post_destroy`                  | warn                       | the tree is gone; only cleanup remains                                                         |
| `session_start` / `session_end` | warn                       | never gate the UI on a hook                                                                    |

Per-entry `on_failure = "block" | "warn"` may tighten or relax within these
bounds — except repo-sourced entries, which are pinned to warn (below).

## Execution contract (`hook_run.rs`)

- `sh -lc <command>`, spawned off the event loop (create/destroy already run
  on off-loop workers; session hooks spawn a `Background`-QoS thread), with a
  per-hook timeout (default 120 s, `timeout_secs` per entry). On timeout the
  process group is killed and treated as a failure.
- Env: the **curated base env** (the same clear-then-allowlist infrastructure
  base panes get) plus `THEGN_EVENT`, `THEGN_REPO_ROOT`, `THEGN_WORKTREE`,
  `THEGN_BRANCH`, `THEGN_WORKSPACE`. Notably _not_ thegn's full process env:
  today `run_prepare` inherits it, which hands every user-shell token to any
  hook; user-level hooks arguably deserve that, but one uniform curated env
  keeps repo-approved hooks from inheriting credentials by default and
  matches the pane model. A user hook that needs a token re-admits it the
  same way panes do (bundles / env_passthrough).
- Output: captured to `$XDG_STATE_HOME/thegn/hooks/<worktree-slug>/<event>-<n>.log`,
  last lines included in the failure notification. Never streamed to the
  loop.
- Blocking hooks run inline in the off-loop worker that owns the operation
  (the create wizard worker, the destroy handler's worker); warn hooks are
  awaited only for status accounting. Completion/failure notifications ride
  the existing refresh channel **with a `TerminalWaker` pulse** — no new wake
  path, no ticker; damage is `Full` via ordinary chrome dirtying.
- Cgroup: hook processes join the shared `thegn.slice` via
  `wrap_background_argv`, like the fold gate and agent handoff — hooks must
  not escape the `[sandbox.limits]` ceilings.

## Back-compat: `prepare` and `initializeCommand`

`[sandbox] prepare` becomes a documented alias whose entries execute as the
head of `post_create` with `on_failure = "warn"`, `wait = false` — behaviour
identical to today except failures surface instead of vanishing.
`run_prepare` is retired into `hook_run`. The devcontainer overlay's
`initializeCommand → prepare` mapping (see `complete-devcontainer-support`)
lands on the same alias, so its one-time host hook point is unchanged.

## Security (load-bearing)

Repo `.thegn.toml [hooks]` is attacker-authored until trusted — it is
arbitrary host-side code execution on clone-open, the exact hole
`add-config-trust-resolution` closed for `prepare`/`init_script`. Rules:

- **Gated**: repo hook entries are `GatedRequest`s (categories
  `hooks.<event>`), canonical-form matched so edits re-prompt, through the
  same TOFU flow and `repo_trust` table. Unapproved ⇒ not run, surfaced
  pending, the operation proceeds.
- **A repo hook can never block**: neither an _unapproved_ hook (it simply
  does not run) nor an _approved-but-failing_ one — repo-sourced entries are
  pinned to `on_failure = "warn"`, and blocking semantics
  (`pre_create` abort, `pre_destroy` hold) are reserved for global/workspace
  scopes. A cloned repo must not hold worktree creation or removal hostage
  (denial-of-removal is an attack: a `pre_destroy` that always fails would
  make a hostile clone undeletable-by-default).
- **Curated env** (above): approved repo hooks do not inherit thegn's process
  env, so approval of "run pnpm install" is not silently also "read my
  GH_TOKEN".
- **Blast radius**: no new external door (no CLI verb/API surface — nothing
  added to the capability catalog); the new write surface is the hook log
  directory under the state dir. Timeouts + the shared slice bound runaway
  hooks.

## Alternatives considered

- **Committed hook files** (`.thegn/hooks/post-create.sh`, the Superset
  fallback style): rejected as the primary surface — config-list entries pass
  through `config_resolve`'s existing classification/gating machinery
  unchanged, while a directory of scripts would need a parallel trust path.
  A config entry can invoke a committed script explicitly, which then rides
  the same gate.
- **Blocking `pre_destroy` everywhere** (including unattended reclaim):
  rejected — wedges the merge queue on a dead script; the queue's job is to
  converge.
- **Streaming hook output into a pane**: rejected for v1 — hooks are
  non-interactive; logs + notification tails cover the debugging need without
  a new pane type.
- **DB-persisted hook run history**: deferred — transient + log files
  suffice; a `state-db` table can come with a UI that reads it.

## Open questions

- Should `session_end` also fire on UI detach with the daemon keeping panes
  alive (currently: no — panes live on, the session is not over)? Revisit
  with `make-daemon-default`.
- A `thegn hooks run <event>` debugging verb (would need a catalog row) —
  left out until asked for.
