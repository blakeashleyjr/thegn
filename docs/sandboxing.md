# Sandboxing

## Git inside sandboxes

`[sandbox.git]` decides what a sandboxed pane (or a provider VM) may do to the
repository's shared `.git`. Configure it globally, or per environment and
profile with `[env.<n>.sandbox.git]` / `[profiles.<n>.sandbox.git]` (present keys
win, absent keys inherit).

### Modes

| `mode`                  | Shared `.git` inside the sandbox                                                                                                                                                                                              |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `read-only`             | Read-only; the worktree's `.git` file is re-pinned read-only.                                                                                                                                                                 |
| `commit-only` (default) | Read-only, except `objects/`, the lane's worktree metadata (`worktrees/<name>`: HEAD, index, logs), and `refs/heads/<lane parent>/`. `hooks/`, `config`, `packed-refs`, `refs/heads/main` and every other ref stay read-only. |
| `push-lane`             | The same mounts as `commit-only`; pushes of `push_refs` go through thegn, not `.git` writes.                                                                                                                                  |
| `full`                  | The commit-only mounts, and the only mode that can pass a git credential.                                                                                                                                                     |

### Fail-closed fallbacks

Outside `mode = "full"`, these cases fall back to a read-only `.git` (with a
warning) instead of a writable one:

- the branch is top-level (no `/`), so a writable `refs/heads/` would expose
  every top-level branch;
- the worktree is the main checkout;
- the merge target is unknown, or the writable ref directory would contain the
  merge target, the default branch or a `deny_refs` entry.

### Tokens

`GH_TOKEN`, `GITHUB_TOKEN` (and GitLab / GitHub Enterprise token variables) are
withheld from sandboxes and provider VMs by default. Only `mode = "full"` with
`credential = "repo-token"` or `"user-token"` passes them. `deploy-key` and
`github-app` parse but are not wired yet.

### Bind destinations and identity

Binds are kept or dropped fail-closed. A symlinked destination is never
rewritten. A bind whose destination path has a symlink below `$HOME` is
dropped with a warning, and the sandbox launches without it. `$HOME` itself
may be reached through a system link.

Sandboxed commits carry the host's git identity, passed as `GIT_AUTHOR_NAME`,
`GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL`. Identity
is not a credential. A value already exported on the host or set in the
sandbox env wins.

### Provider worktrees: bundle transport

With `transport = "bundle"` (default), a provider worktree is seeded from a git
bundle uploaded over the provider files API, so no GitHub credential enters the
VM. Bring commits back into the lane branch with:

```sh
thegn env pull
```

Pull-back refuses `main`, the default branch and the merge target, applies
`deny_refs`, and fails closed when the default branch is unknown.
`thegn env down` pulls back first, then destroys the provider sandbox and
verifies it is gone before reporting "down". If the pull-back fails it
refuses; `--force` destroys anyway and unpulled commits are lost.
`thegn env down --stop` pulls back and parks the sandbox (stop or suspend)
instead of destroying it. When the last session for a provider worktree
exits, the daemon pulls back once in the background.

### Residuals

These are accepted limits of the commit-only mounts:

- Lanes share the `refs/heads/tg/` directory, so a sandbox is protected from
  `main`, tags and non-`tg/` branches but not from sibling lanes. A sibling ref
  can be rewritten into a symref, so host code that later updates a lane ref
  must not dereference symrefs.
- The object store is shared and writable (existing object files can be
  rewritten in place), so ref protection is not content integrity.
- Deleting a branch whose ref is packed fails, because `packed-refs` is
  read-only.
- The per-worktree `HEAD` is writable and can become a symref to
  `refs/heads/main`; host code that later commits, merges or fast-forwards in
  that worktree must verify `HEAD` is the lane branch first.
- The writable index and worktree let a sandbox plant a gitlink whose
  `<sub>/.git` points at its own git dir. Host git in a sandboxed worktree must
  run with `--ignore-submodules` and `-c core.fsmonitor=false`.

The runtime directory and the daemon's control socket are masked inside
sandboxes, so a sandbox cannot reach them (THE-2009, THE-2011).
