# Add remote provision hooks (bring-your-own-infra for sprites)

## Summary

Let users bring their own remote infrastructure to thegn's remote "sprite"
workspaces via **user-authored per-task provision and teardown scripts**, modeled
on [Emdash Cloud](https://emdash.sh/cloud)'s "Bring Your Own Infra" provision/
teardown contract. Beyond the static per-provider `up_command`/`down_command`, a
task can run an ordered set of hooks — passed the task's id, repo, and branch —
that bring up (and tear down) an arbitrary VM / GPU box / container, with hook
output surfaced in the UI and a timeout per hook.

## Impact

- **Sprites / placement** — extends `Placement`'s `ensure()`/`teardown()`
  lifecycle with task-scoped user hooks, so remote execution isn't limited to the
  provider's built-in up/down.
- **Lifecycle / warm pool** — hooks compose with the existing `[lifecycle.pool]`
  and `claim_pool_spare` path (a claimed spare skips provision; a fresh task runs
  it).
- Extends the `sandbox` and `terminal-hosts` capabilities. **DB schema change:
  possible `user_version` bump** if per-task hook results are persisted for the UI
  (otherwise transient).

## Rationale

thegn already models remote placement (`Placement { Local | Ssh | K8s |
Provider }` with `ensure`/`teardown`/`port_forward_argv`) and a provider's static
`up_command`/`down_command`. Emdash's edge is a _user-authored_ per-task
provision/teardown contract plus a workspace-path convention, which lets power
users target GPU boxes and arbitrary VMs without waiting for a first-class backend.
Given thegn's existing sprite + pool investment, exposing a documented per-task
hook contract (with task context, ordering, timeout, and captured output) is a
small, high-leverage addition that stays entirely in the shell/infra layer.

## Non-goals

- **A managed cloud offering** — thegn runs the user's own infra via their
  scripts; it is not Emdash Cloud's hosted compute.
- **Replacing provider up/down** — static provider commands remain; hooks are the
  richer, task-scoped extension layered on top.
- **Arbitrary in-loop execution** — hooks run off the event loop with a timeout;
  they never block the render loop.
- **AI dependency** — provisioning is pure infra plumbing; no proxy/agent
  involvement.
