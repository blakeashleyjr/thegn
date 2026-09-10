# Design — identity-bound commit signing

## Execution boundary

The identity scope fold already resolves profile, bundle, global, workspace,
and worktree bindings for every local pane. Extend that one fold with a typed
Git-signing result, then lower it into the child Git environment at launch.
Do not edit `GIT_CONFIG_GLOBAL`: it is user-owned and may be shared by sibling
worktrees. Do not put private key contents in environment variables; only Git's
format and public key path/key id are configuration values.

Both host and sandbox launch paths must consume the same resolved structure.
Remote-provider panes need an explicit feasibility decision because a host path
is not automatically meaningful in the remote filesystem; they must either
mount/provision the selected key safely or reject the binding with an
actionable message.

## Precedence

Identity scope resolves low to high: profile base, bundle chain, explicit
global, workspace, then worktree binding. Per-operation policy is higher:

1. `[git] override_gpg` disables signing for background history rewrites.
2. The commit overlay's inherit/sign/no-sign choice controls its one commit.
3. The resolved identity provides the default format/key and the accepted
   automatic-enable policy.
4. An unbound worktree inherits repo/global Git configuration.

Tests must execute Git to prove this order rather than only compare constructed
vectors.

## Policy decision required

The accepted requirement says a worktree bound to a signing identity produces
signed commits without per-commit flags. Before implementation, choose and
document one of these compatible mechanisms:

- identity signing implies `commit.gpgSign=true`, with operation-level no-sign
  able to suppress it; or
- add an explicit `enabled` field whose accepted default makes the scenario
  true while retaining a key-selection-only mode.

Silently depending on ambient `commit.gpgSign` does not satisfy the scenario.

## Verification

Use an isolated temporary SSH signing key and repository. Prove a bound
worktree's commit contains a valid `gpgsig`, its unbound sibling follows the
fixture's repo/global behavior, and an operation-level no-sign override yields
an unsigned commit. Run the same resolved environment through host and sandbox
launch composition; skip an external executable only with an explicit reason.
