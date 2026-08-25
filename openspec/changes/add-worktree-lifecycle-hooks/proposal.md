# Add worktree lifecycle hooks (pre/post create/destroy + session)

Linear: THE-19

## Why

thegn's only lifecycle hooks today are `[sandbox] prepare` (host-side commands
at worktree creation) and `init_script` (in-sandbox, per pane). Both are
narrow and the first is unaccountable: `sandbox::run_prepare` is a
fire-and-forget thread — no ordering guarantee versus the first pane (it races
it), no output capture, no timeout, no failure surfacing, nothing at destroy
time, and nothing at session boundaries. Every worktree teardown path
(`cmd/wt.rs` rm, the sidebar delete, `merge_lifecycle` auto-cleanup,
`workspace_remove`) removes the tree with no chance to stop a dev server,
drop a database, release a port, or archive artifacts. Roadmap item D 54
(worktree templates) explicitly records "setup/post-create hook depth still
partial", and comparable tools (Superset's setup/teardown scripts) treat this
as table stakes for a worktree-per-task workflow.

## What Changes

- **A `[hooks]` table** with ordered command lists per event, configurable at
  global scope, `[workspace.<slug>.hooks]`, and repo `.thegn.toml [hooks]`
  (repo-authored entries are trust-gated — see Security):
  - `pre_create` — host-side, before `git worktree add`; a failure **blocks**
    creation (the tree does not exist yet; aborting is safe and honest).
  - `post_create` — host-side, after the worktree is registered and after
    built-in env provisioning (prepare/direnv warm/devshell resolve); failure
    **warns**. Runs in parallel with the first pane by default; a `wait` flag
    makes the first pane wait (Superset's "wait for setup" behaviour).
  - `pre_destroy` — host-side, before `git worktree remove`; on failure a
    user-invoked destroy **blocks with a force override** (the Superset
    force-delete pattern); unattended reclaim (merge-lifecycle cleanup)
    **warns and continues** — a dead teardown script must not wedge the queue.
  - `post_destroy` — after removal; failure **warns**.
  - `session_start` / `session_end` — host-side, when a worktree's first pane
    of a UI session spawns / its last pane exits or the tab closes; failures
    **warn**.
- **One execution contract**: hooks run `sh -lc` off the event loop, cwd =
  the worktree (repo root for `pre_create`/`post_destroy`, where the worktree
  path may not exist), with `THEGN_EVENT`, `THEGN_REPO_ROOT`,
  `THEGN_WORKTREE`, `THEGN_BRANCH`, `THEGN_WORKSPACE` in the environment, a
  per-hook timeout (default 120 s), sequential order within an event
  (global → workspace → repo), captured output written to a state-dir log and
  surfaced via notification on failure. Entries are either a command string or
  a table (`{ command, wait, timeout_secs, on_failure }`) overriding the
  event's defaults where the semantics allow (a repo entry can never escalate
  to blocking).
- **`[sandbox] prepare` folds in** as the head of `post_create` (kept as a
  documented back-compat alias; existing configs behave identically except
  failures now warn instead of vanishing).
- **Trust gating for repo-authored hooks**: `.thegn.toml [hooks]` entries join
  the trust-on-first-use gated class from `add-config-trust-resolution`
  (categories `hooks.<event>`), with two hard safety rules: an unapproved
  hook never runs _and never blocks_ the operation, and a repo-sourced hook
  failure never blocks either (`pre_create`/`pre_destroy` blocking semantics
  are reserved for user-level hooks) — a cloned repo must not be able to hold
  worktree creation or removal hostage.
- **Doctor line**: configured hooks per event, their source scope, and repo
  hooks' trust state.

## Impact

- **tasks.md**: D 54 (worktree templates' setup/post-create hooks — this is
  the hook substrate it needs), D 47/49 (delete flow gains pre_destroy), O
  (configuration).
- **Specs**: extends the `workspace` capability (worktree lifecycle —
  ADDED requirements) and modifies the `sandbox` capability's
  trust-on-first-use requirement to include the `hooks.*` categories.
- **In-flight changes**: builds on `add-config-trust-resolution` (extends its
  gated category set; no new trust machinery). `add-remote-provision-hooks`
  is the _remote sprite_ provision/teardown story — this change is the local
  worktree lifecycle; the execution contract (context env, ordering, timeout,
  captured output) is deliberately the same shape so the two converge, and
  remote worktree hooks are explicitly out of scope here.
  `add-issue-driven-worktrees` and the worktree wizard both create through
  the same path, so they inherit hooks with no extra wiring.
  `complete-devcontainer-support` maps `initializeCommand` onto the host-side
  one-time hook point; folding `prepare` into `post_create` keeps that
  mapping intact.
- **Capability catalog**: no new externally invokable operation — hooks run
  internally on lifecycle events; no new CLI verb, no catalog row.
- **DB**: no schema change — hook results are transient (log files under the
  state dir); trust approvals reuse the `repo_trust` table.
- **Code**: `thegn-core` — `hooks.rs` (config types, event model, ordering,
  failure-policy resolution, env contract; pure + unit-tested),
  `config_resolve.rs` (gated classification for repo `[hooks]`); `thegn-host`
  — `hook_run.rs` (off-loop executor: timeout, capture, notification,
  waker), call sites in `wizard.rs` (create), `cmd/wt.rs`,
  `handlers/workspace_remove.rs`, `merge_lifecycle.rs`, `run.rs` (destroy +
  session boundaries); `run_prepare` retired into the new executor.

## Non-goals

- **Remote sprite provision/teardown** — `add-remote-provision-hooks`' scope.
- **Git hooks** (`pre-commit`, `pre-push`) — unrelated; those belong to git
  and the repo's own tooling.
- **A veto/confirmation UI for hooks beyond block-with-force** — no
  interactive stdin to hooks; they are non-interactive commands.
- **Per-hook shells or interpreters** — `sh -lc` only, like every other
  command surface in thegn.
