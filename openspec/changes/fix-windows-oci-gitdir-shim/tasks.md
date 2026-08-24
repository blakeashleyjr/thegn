# Tasks — OCI on Windows: git metadata

## 1. Correctness fixes found by actually running it

- [x] 1.1 `sandbox_preflight::preflight_exec_argv` maps `--workdir` through
      `container_path`, so it really does mirror the OCI arm of
      `backend_enter_argv` as its doc comment claims. Was sending a raw `C:\…`
      path, so the probe failed and left an orphaned container behind on every
      pane spawn.
- [x] 1.2 `sandbox::container_status` compares `inspect .Source` against
      `container_path(m.host)`. Comparing the raw host path can never match on
      Windows, so `mounts_ok` was permanently false and every spawn
      force-recreated a healthy container.
- [x] 1.3 `sandbox_mounts`: map every `dest` through `container_path`
      (`map_dests`). The host-toolchain, cache and carve-out builders are
      path-preserving by design; on Windows that emitted
      `-v C:\Users\you:C:\Users\you` and **the container never started**. Only a
      real `ensure()` surfaced this.
- [x] 1.4 `FileAccess::All | Host` uses `volume_root` instead of `Mount { "/", "/" }`.
      Windows has no single filesystem root, so `/` is meaningless there.
- [x] 1.5 Unit test for `volume_root` (drive on Windows, `/` elsewhere).

## 2. `sandbox_gitshim`

- [x] 2.1 New `crates/thegn-core/src/sandbox_gitshim.rs`:
      `plan(worktree, git_common, name)`, the injectable-pure `plan_with`, and
      `materialize(files)`. Registered in `lib.rs`.
- [x] 2.2 Gated on `container_path(p) != p`, never `cfg!(windows)`, so a Windows
      thegn driving an ssh placement onto Linux correctly no-ops.
- [x] 2.3 Rewrite anchored on `container_path(git_common)` with a
      case/separator-insensitive `starts_with`, so the shim and the emitted `-v`
      agree even when git and `loc.path()` disagree on drive-letter case.
- [x] 2.4 Emits BOTH the `.git` pointer and the `gitdir` back-pointer — measured:
      shimming only `.git` still yields `not a git repository`, so the
      back-pointer is load-bearing for *resolution*, not merely for prune. Plus
      `commondir` only when the host's is absolute, and an extra `Mount` for a
      `--separate-git-dir` gitdir.
- [x] 2.5 ro `<git-common>/worktrees` + rw `<git-common>/worktrees/<id>` appended
      in `sandbox::add_worktree_mounts`, after the existing binds.
- [x] 2.6 Planned files travel on the spec (`SandboxSpec::gitshim_files`) and are
      written by `ensure`. Deliberately not re-planned at write time: a re-plan
      could anchor on a differently-shaped `git_common` and silently write a
      pointer to somewhere nothing is mounted. Failure is fatal, not
      best-effort — the mounts already reference those paths.
- [x] 2.7 `-c worktree.useRelativePaths=true` on thegn's own `git worktree add`
      (`worktree.rs`). Verified locally: produces relative `.git` *and* `gitdir`
      pointers, which the shim then correctly skips.
- [x] 2.8 Nine table tests in `sandbox_gitshim.rs`, all runnable from Linux
      because `plan_with` takes the mapping as an argument: identity mapping,
      relative pointer, the linked-worktree case, sibling protection + mount
      ordering, relative vs absolute `commondir`, `--separate-git-dir`,
      drive-case/separator mismatch, main checkout, and `materialize`.
- [x] 2.9 Gate lifted in `backend_suitable_on`; the regression test now asserts
      the *coupling* — OCI eligibility everywhere, plus an assertion that the
      shim still plans mounts, because eligibility is only sound while it does.

## 3. End-to-end verification (the thing that was skipped last time)

- [x] 3.1 New `tests/sandbox_gitshim_e2e.rs` (Tier 2, `PODMAN_E2E_FORCE`): real
      repo, two real linked worktrees with absolute pointers, real `resolve` →
      `ensure` → `podman exec`.
- [x] 3.2 Asserts git resolves in the sandbox, `worktree list` reports mapped
      paths, a commit made inside lands in the host's repo, and the host's own
      pointer file is untouched.
- [x] 3.3 Asserts an in-sandbox `git worktree prune` cannot reach sibling tabs.
      Driven through the mounted git-common dir, not the worktree — from the
      worktree the test would pass vacuously, because with the shim removed git
      there fails before prune ever runs.
- [x] 3.4 **Both cases verified to FAIL with the shim disabled**:
      `not a git repository: (null)`, and a sibling worktree actually destroyed
      (`Removing worktrees/wt: gitdir file points to non-existent location`).
- [x] 3.5 Fixed `sandbox_lifecycle.rs` d1, which hardcoded a container name but
      tore down by *path* — it removed a container that never existed, so the
      teardown assertion was vacuous and it leaked a container per run.

## 4. Docs & spec

- [x] 4.1 This change folder (proposal, design, delta specs).
- [x] 4.2 `tasks.md` AX 737 corrected — both halves of its claim were false.
- [x] 4.3 `docs/windows-native-audit.md`: the measured podman results, including
      the retired risks and the things that turned out fine.
- [x] 4.4 Stale reasons removed from `backend_suitable`'s doc, the now-dead
      `unsuitable_reason` branch, and `config_defaults::default_backend_chain`.
- [ ] 4.5 `just openspec-validate` — needs `nix develop`; the openspec CLI is not
      available on the Windows dev box, so this must run on Linux before the PR.

## 5. Known gaps (deliberately not fixed here)

- [ ] 5.1 `sandbox_mounts::parse_mount` splits `[sandbox] mounts` entries on
      `:`, so a Windows path (`C:\data`) mis-parses into host `C` / dest
      `\data`. Pre-existing and independent of this change; a correct fix needs
      a decision on how a Windows mount is spelled in config.
- [ ] 5.2 `ensure` keeps containers alive with `<image> sleep infinity`, which
      breaks for any image declaring an `ENTRYPOINT` (e.g. `docker.io/alpine/git`
      runs `git sleep infinity`). Passing `--entrypoint ""` would be strictly
      more robust; it is a behaviour change, so it is called out rather than
      slipped in.
- [ ] 5.3 A Windows host with the default `core.autocrlf=true` shows every file
      as modified inside a Linux container. Real wart of OCI-on-Windows; needs a
      product decision, not a bug fix.
