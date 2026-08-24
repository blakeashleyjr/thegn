# Tasks

## 1. Verify the bind from inside the container

- [x] 1.1 New pure module `thegn-core/src/sandbox_mountcheck.rs`:
      `mount_sentinels` (host-observed paths only), `preflight_probe_body`
      (literal `true` when nothing is provable), `parse_missing_sentinel`,
      `parse_unshared_bind`, `mount_failure` + `MountFailure::one_line`.
      Not placed in `sandbox_preflight.rs`, which is inside the `cov_ignore`
      regex and would let the logic escape the 95% core gate.
- [x] 1.2 Ride the existing preflight probe: split `preflight_exec_argv` into a
      seam plus pure `preflight_exec_argv_with(spec, body)`; classify a probe
      failure as a mount failure vs a generic runtime error.
- [x] 1.3 Extract the twice-duplicated force-remove into `remove_container` and
      call it on a verified mount failure — binds are fixed at create, so
      `container_status` would call the broken container healthy forever.
- [x] 1.4 Keep the create's stderr (`stderr_with_timeout`) and bail with the
      diagnosis, skipping the `--userns keep-id` retry: that retry varies the
      user namespace, not the mount set, so it only rediscovers the next
      unshared path.
- [x] 1.5 Key the remedy on the **missing** path's share root, not the
      worktree's, for the cross-volume case.
- [x] 1.6 Unit tests (17): sentinel derivation, the unprovable-spec no-regression
      lock, dest-vs-host remapping, stderr parsing for both runtimes' phrasing,
      and the full remedy matrix incl. the Linux-regression guard.

## 2. Report macOS isolation honestly

- [x] 2.1 `capabilities::from_parts_on(.., os)` + an `isolation_for` arm placed
      after `runsc`/`krun` so a stronger runtime still wins.
- [x] 2.2 `doctor::isolation_of_on(.., os)`; tests pinned per-OS so the Linux
      answer is asserted from a Mac and vice versa.

## 3. Close the ABI gate's remaining holes

- [x] 3.1 `guest_shares_host_abi(backend, os)` — one predicate, replacing the
      ad-hoc expression that covered only the toolchain mounts.
- [x] 3.2 Apply it to the Tier-B nix-daemon socket (with a warning when
      `nix_daemon` was explicitly requested) and the devenv `/nix` bind.
- [x] 3.3 Tests: the predicate's full backend × OS matrix, and an argv-level
      assertion that `/nix` never reaches a foreign guest.

## 4. Verification on real runtimes (macOS 26)

- [x] 4.1 podman 5.8.6 — refuses an unshared bind loudly (exit 125, no
      container); confirmed the create stderr now carries path + remedy.
- [x] 4.2 docker 29.5.2 via colima — **silently** mounts an empty directory;
      confirmed the in-container probe catches it, names the colima-specific
      remedy, and removes the container.
- [x] 4.3 No-regression: worktree inside the share starts and shows all files
      under podman, docker and Apple `container`.
- [x] 4.4 `thegn doctor` on macOS reports guest-kernel for podman/docker/apple.
- [ ] 4.5 Docker Desktop and Apple `container` refused-bind phrasings are not
      matched by `parse_unshared_bind` — deliberately unguessed; they fall
      through to the generic error, which is today's behaviour.

## 5. Gates

- [x] 5.1 `cargo nextest run --workspace` (4928 passed).
- [x] 5.2 `cargo fmt --all` + `cargo clippy --workspace --all-targets -D warnings`.
- [x] 5.3 `just smoke`.
- [ ] 5.4 `just ci` — blocked on a Mac: all muse e2e baselines are `__linux`
      and `--ci` treats a missing baseline as failure (owned by the parallel
      muse effort). `coverage` needs `nix develop`.
