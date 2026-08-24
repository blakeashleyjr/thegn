# Fix OCI-on-Windows: git metadata, not mount paths

## Summary

Commit `ba53e8c6` removed the native-Windows OCI decline after adding
`sandbox::container_path` (which maps `C:\…` → `/mnt/<drive>/…` for mount
destinations and `--workdir`). Mount destinations were indeed the *stated*
blocker in the old requirement — but they were not the only one, and the change
shipped with no delta spec, so a normative requirement was contradicted silently.

Re-measured against a real Podman Desktop install (WSL-backed machine), the
un-declined path is **strictly worse than the decline it replaced**:

| Behaviour | Measured result |
| --- | --- |
| `git` in a sandboxed linked worktree | `fatal: not a git repository: (null)` |
| Preflight probe (`sandbox_preflight.rs`) | `crun: chdir to 'C:\…': No such file or directory` — it passed the *raw* host path while the pane arm mapped it |
| Container after a failed preflight | still `Up` — one orphan per worktree, recreated every spawn |
| `git worktree prune` inside the sandbox | reports **every sibling tab's** metadata as prunable and deletes host state |
| `container_status` mount check | thegn stores `C:\Users\…\wt`, `inspect .Source` reports `/mnt/c/Users/…/wt` — never matches, so the container is force-recreated on every pane spawn |

Root cause: every thegn tab is a **linked** worktree, whose `.git` is a pointer
file carrying an **absolute host path**. `container_path` fixed where things are
mounted; it did not fix what git reads once it gets there.

This change fixes all of it, and the gate stays lifted only because the fix is
verified end to end against a real podman rather than reasoned about:

1. **The unmapped-destination bugs** — the preflight `--workdir`, the
   `container_status` bind comparison, the host-toolchain/cache mounts, and the
   `file_access = all/host` root mount. Two of those were invisible until a
   container was actually created: an unmapped toolchain destination emits
   `-v C:\Users\you:C:\Users\you`, which the runtime rejects, so the container
   never starts at all.
2. **`sandbox_gitshim`** — bind a rewritten `.git` pointer and `gitdir`
   back-pointer so git resolves under the mapped destination, plus a read-only
   `<git-common>/worktrees` with the pane's own entry overmounted read-write.
3. **`tests/sandbox_gitshim_e2e.rs`** — a Tier-2 suite that resolves a real
   spec, ensures a real container, and execs git inside it. Both cases were
   checked to FAIL with the shim disabled (`not a git repository: (null)`, and a
   sibling worktree actually destroyed), so neither can pass vacuously.

## Why now

This is a security- and data-integrity-relevant claim. Before this change, any
user who installed Podman or Docker Desktop on Windows got a sandbox that could
not run git and could delete the git metadata of every other tab.

## Impact

- **tasks.md AX 737** (`tasks.md:1491`) — its text claimed "OCI backends declined
  on Windows with the same-path/WSL2 warning + `jobobject` in the default chain".
  Both halves were false: the same-path reason was never the real blocker, and
  `jobobject` was separately made to probe `Absent` because nothing ever assigns
  a pane to a Job Object. Updated to state what is now true.
- Specs: `platform-windows` (the decline requirement becomes a conditional
  eligibility one, and the `jobobject` claim is split out of it), `sandbox` (the
  bind-mount requirement generalized from a *mechanism* to an *invariant*, plus
  sibling-metadata protection).
- No user-facing behaviour change on Linux or macOS. `container_path` is the
  identity there and the new mounts are gated on the mapping being non-identity.
