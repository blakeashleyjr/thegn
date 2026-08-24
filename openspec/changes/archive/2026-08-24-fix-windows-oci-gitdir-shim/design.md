# Design — OCI on Windows: the gitdir shim

## The problem, precisely

A linked worktree's `.git` is a pointer file. Measured on a real repo:

```
gitdir: C:/Users/…/repo/.git/worktrees/wt          # <wt>/.git
C:/Users/…/wt/.git                                 # <common>/worktrees/wt/gitdir
../..                                              # <common>/worktrees/wt/commondir
```

`commondir` is **relative** (git writes `../..`), so it resolves under any
mapping for free. The other two are absolute host paths and do not.

Two consequences inside a Linux container mounted at `/mnt/c/…`:

1. `.git` points at a path that does not exist ⇒ `fatal: not a git repository`.
2. `<common>/worktrees/<id>/gitdir` is not `is_absolute_path()` on Linux, so
   git's `should_prune_worktree` classifies the entry as prunable **before** the
   `--expire` gate. Since `<git-common>` is mounted whole, `worktrees/` holds
   every sibling tab's metadata — one `git worktree prune` takes them all.

## Chosen mechanism: bind a rewritten pointer

Write two small files on the host (under `util::thegn_dir()`, named from the
sandbox name so the path is deterministic at spec-resolve time) and bind them
**read-only** over the container's view:

| shim file | mounted at | contents |
| --- | --- | --- |
| `dotgit` | `<wt_dest>/.git` | `gitdir: <mapped gitdir>` |
| `gitdir` | `<mapped gitdir>/gitdir` | `<wt_dest>/.git` |
| `commondir` | `<mapped gitdir>/commondir` | only when the host's is absolute |

**Both of the first two are mandatory.** Measured: shimming only `.git` and
leaving the original `gitdir` back-pointer still yields `not a git repository`.
The back-pointer is load-bearing for *resolution*, not merely for prune.

Read-only is deliberate: git rewrites these only via `worktree repair`/`move`,
which thegn owns on the host. A container-side attempt then fails loudly instead
of silently diverging — the same reasoning as the existing `.git/config` pin.
Verified that a file bind does **not** write through to the host file.

### Anchoring

Build the rewrite from the **mount destination**, not an independent mapping: if
`pointer.starts_with(git_common)` (case-insensitively on Windows), splice
`container_path(git_common)` onto the remainder. This guarantees agreement with
the emitted `-v` even when git and `loc.path()` disagree on drive-letter case or
separators.

### Gating

Gate on `container_path(pointer) != pointer`, **not** `cfg!(windows)`. A Windows
thegn driving an SSH placement onto Linux sees POSIX paths, which the mapping
leaves alone, so the shim correctly no-ops — which is also what the spec's
placement carve-out requires.

## Sibling protection

`<git-common>/worktrees` ro, with `<git-common>/worktrees/<id>` overmounted rw.
This is the idiom already in the file (parent bind first, child bind after — the
`.git`(rw) → `.git/config`(ro) pin). Verified against podman that the runtime
applies both with the intended flags, that a sibling's metadata survives an
in-container `git worktree prune` (`error: failed to delete …: Read-only file
system`), and that the pane's own commit still succeeds through the rw child.

Residual, accepted: a pane can still destroy *its own* worktree metadata. It owns
it. And `git worktree add` from inside a sandbox now fails — the same workflow
that produced the `core.worktree` incident.

## Rejected alternatives

- **`GIT_DIR`/`GIT_WORK_TREE` injection.** Structurally forbidden here:
  `wrap_script` emits `unset GIT_DIR` &c. at the top of every pane script, and
  `util::GIT_ENV_VARS` documents that scrub as the fix for the `core.worktree`
  pollution incident. It is also a per-process-tree fix for a per-directory
  problem — it breaks the moment the user `cd`s into another repo in the pane.
- **`worktree.useRelativePaths` alone.** Complementary, not sufficient: needs git
  ≥ 2.48, falls back to absolute when worktree and repo are on different drives,
  and retrofitting existing worktrees means running `git worktree repair` — i.e.
  mutating the user's repo as a side effect of *starting a pane*. Worth setting
  on the creation path as belt-and-braces; not the mechanism.
- **Mounting the real gitdir at a container path literally named `C:`** so the
  unmodified pointer resolves. It would work, and it puts a directory named `C:`
  inside the user's working tree where `git status` reports it. No.

## Notes for implementation

- `plan()` is pure-ish (reads host pointers via the already-tested
  `gitdir::{parse_dotgit_pointer, resolve_pointer, local_git_common_dir}`) and
  returns mounts + file contents; `materialize()` is the side effect and is
  called from `sandbox::ensure` so `resolve_placed` stays pure and testable.
- `--separate-git-dir` repos put the gitdir outside `<git-common>`; detect with
  `!gitdir.starts_with(git_common)` and add an extra `Mount`.
- `safe.directory` needs no work — `wrap_script` already emits
  `git config --global --add safe.directory '*'`. Every passing experiment above
  depended on it; without it git refuses the bind-mounted worktree as
  dubiously-owned (uid differs across the 9p/drvfs mount).
- `FileAccess::All | Host` currently pushes `Mount { host: "/", dest: "/" }`,
  which is meaningless on Windows. Latent while OCI is declined there; must be
  handled before the gate lifts.
