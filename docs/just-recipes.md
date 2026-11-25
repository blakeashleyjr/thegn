# Just recipe catalog

Generated from a full read of `justfile` and the scripts it calls (2026-11-24,
alpha.130). `just --list` shows what exists; this page says what each recipe is
for, which ones compile, and which ones are suspect. No recipe has been deleted.

**Heavy** means the recipe compiles or runs the cargo suite. `test/heavy-guard.sh`
(wired in `.claude/settings.json`) blocks agents from running `just test`,
`test-doc`, `test-windows-wine`, `ci`, `ci-local`, `coverage`, `coverage-html`,
`lint`, `bench`, `bench-micro`, `e2e`, `doc-check` and any `--workspace` cargo
build/check/clippy/test on the workstation; `THEGN_ALLOW_HEAVY=1` overrides it.
The guard list is shorter than the set of compiling recipes: `build`,
`release`, `check-cross`, `check-features`, `check-msrv`, `smoke`, `nix-build*`,
`ratchets-all`, the `bench-*` family and the rest of the `Mac` rows below also
compile, so treat every `Mac` row as off-limits locally. Iterate with
`just quick <crate>` or `cargo nextest run -p <crate> <filter>` on the Mac.

## Common tasks

| Task                                | Command                                                                                                                                                                                                            |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Start the dev instance              | `just live` (release build, rebuilds only when inputs changed). `just live-plan` previews. `--here` starts in this pane, `--restart-daemon` replaces the dev daemon.                                               |
| Start the dev instance, debug build | `just devel`                                                                                                                                                                                                       |
| Update the stable instance          | `just update stable` (latest verified release), `just update stable 0.1.0-alpha.130`, `just update stable --dry-run`, `just update stable --rollback`                                                              |
| Update the dev instance             | `just update dev` (latest release via `dev_live.py --prebuilt`), `just update dev main`, `just update dev checkout`, `just update dev <branch>`                                                                    |
| See what is installed               | `just update` (stable, dev, latest release, behind or not)                                                                                                                                                         |
| Run tests on the Mac                | `just test-remote host=<mac>` (whole gate), `just test-remote host=<mac> <crate> [filter]`; the personal helpers in `~/.local/share/thegn-devbox/` (`mac-check.sh`, `rgate.sh`, `rbuild.sh`) are outside this repo |
| Inner loop                          | `just quick <crate>` and `cargo nextest run -p <crate> <filter>`, on the Mac                                                                                                                                       |
| Cheap lint, no compile              | `just ratchets` (seconds; the source ratchets and grep guards)                                                                                                                                                     |
| Full lint                           | `just lint` (clippy, treefmt, shellcheck, yamllint, taplo, delivery-check; Mac)                                                                                                                                    |
| Format                              | `just fmt`                                                                                                                                                                                                         |
| Pre-push gate                       | `just test`, `just smoke`, `just lint`; `just ci` for everything (Mac)                                                                                                                                             |
| Release                             | `RELEASING.md`: `just third-party-notices`, `just nix-build-full`, `just release-artifacts v<ver>`, `just release-verify v<ver>`, `just rehearse-packages ...`; publish with the personal `release-alpha.py`       |

### `just update`

`update` has one entry point, `scripts/update_instance.py` (stdlib Python, tested
by `test/update_instance_test.py`, wired into `just test` as `test-update-instance`).

- **Stable** is the binary at `~/.local/lib/thegn/thegn`, launched by
  `~/.local/bin/{tg,tg-tui,thegn}`. `update stable` downloads the
  `x86_64-unknown-linux-gnu` tarball and `.sha256` from GitHub releases into a fresh
  temp dir, checks the digest, runs `gh attestation verify` (no `gh`: refuses unless
  `--no-attest`), extracts only the root `thegn` member, runs `--version` and requires
  it to match the tag, then renames it into place atomically and hard-links the old
  binary to `thegn.prev`. It refuses a downgrade without `--force` and does nothing
  when already current. `--rollback` swaps `thegn.prev` back in (and keeps the
  replaced binary as the new `thegn.prev`). `--dry-run` prints the plan and downloads
  nothing.
- **After an install the instance is restarted**, because a newer database schema
  refuses to migrate while a daemon (or client) of the old binary still uses the old
  schema. The recipe finds the instance's processes (`/proc/<pid>/environ`
  `XDG_STATE_HOME` equal to the instance's state dir AND `exe` equal to its binary,
  including the ` (deleted)` form), and prompts once: `Restart the <instance> daemon
now? This closes N open panes (shells, agent workers). [Y/n]` (N from `thegn session
list --json`, else "all open <instance> panes"). `--yes` skips the prompt;
  non-interactive without `--yes` restarts nothing, prints the commands and exits 1.
  It runs `<new binary> daemon stop` with the instance's environment, waits up to 20 s,
  then SIGTERM, then SIGKILL, signalling only pids that still match the instance. Run
  from a pane of that same instance, the stop and relaunch go through a detached helper
  (log: `<state>/update-restart.log`). It then relaunches: dev through
  `scripts/devel-launch.sh --standalone`, stable through `~/.local/bin/tg --standalone`
  (both need alacritty; otherwise it says to run `just live` / `tg`). `--no-launch`
  skips the relaunch. The same restart repairs an instance whose daemon still runs a
  replaced binary when the binary is already current (a plain re-run). `just update`
  (status) lists such daemons. The state database schema only moves forward.
- **Dev** goes through `scripts/dev_live.py --prebuilt <binary>`, so the new binary is
  validated against the dev config before it replaces `~/.thegn-devel/bin/thegn`.
  `release[=VERSION]` (default) uses the same verified download. `main`, `checkout`
  (current branch; refuses a dirty worktree) and `<branch>` build on the remote Mac
  builder (`THEGN_REMOTE_BUILD`, default `~/.local/share/thegn-devbox/rbuild.sh`, optional).
  If that is missing or fails it asks `Mac build failed (<reason>). Build locally
instead? This compiles on this machine. [y/N]`; non-interactive runs refuse, and
  `yes` falls back to the local `dev_live.py` build of the checked-out branch.
  `--level debug` style arguments pass through to `dev_live.py --install-only`; `--restart-daemon` means `--yes`, and `--here` is ignored (the dev instance reopens in its own window).
- `latest` means the highest version among published releases: every release is a
  pre-release, so GitHub's `releases/latest` endpoint returns 404.

**Status:** OK, STALE (references something that no longer behaves as written), DUP (overlaps another recipe), UNCLEAR (purpose, ownership or viability in doubt).

## Build and package

Anything that compiles runs on the Mac (see the guard note below).

| Recipe                  | Params  | Purpose                                                                                                                 | Heavy | Status  |
| ----------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------- | ----- | ------- |
| `build`                 |         | Debug build of the whole workspace, plus `cargo check -p thegn-host --features dev`.                                    | Mac   | OK      |
| `quick`                 | `[pkg]` | Inner loop: clippy on lib/bin only, optionally one crate.                                                               | Mac   | OK      |
| `check-cross`           |         | Typecheck macOS and Windows targets; each leg skips loudly without its toolchain.                                       | Mac   | OK      |
| `check-features`        |         | Every named cargo feature alone and together, plus iroh clippy/tests.                                                   | Mac   | OK      |
| `check-msrv`            |         | `cargo-1.96 check --workspace --locked` (flake MSRV toolchain).                                                         | Mac   | OK      |
| `doc-check`             |         | Rustdoc with `-D warnings --document-private-items`.                                                                    | Mac   | OK      |
| `build-profiling`       |         | Debug host with the `profiling` feature.                                                                                | Mac   | OK      |
| `release`               |         | Release build of the workspace. Despite the name it is only a build, not the release process (see `release-artifacts`). | Mac   | UNCLEAR |
| `release-profiling`     |         | Release host with `profiling` (SIGUSR2 flamegraphs), sccache wrapper disabled.                                          | Mac   | OK      |
| `build-musl`            |         | Static x86_64-musl `thegn` (the Sprites bridge binary) via cargo.                                                       | Mac   | OK      |
| `bridge`                |         | Build the musl bridge with nix and install it as `thegn-musl` next to the debug and release binaries.                   | Mac   | OK      |
| `host`                  | `*args` | Build, then run the debug host with args.                                                                               | Mac   | DUP     |
| `completions`           |         | Generate shell completions for `thegn` and `tg` into target/completions.                                                | Mac   | OK      |
| `nix-build`             |         | Build the Nix package (`./result`).                                                                                     | Mac   | OK      |
| `nix-build-full`        |         | Full default install: host plus musl bridge (two release compiles). Run before a release.                               | Mac   | OK      |
| `path`                  |         | Print the Nix store path without creating `./result`.                                                                   | light | OK      |
| `flake-check`           |         | `nix flake check`.                                                                                                      | Mac   | OK      |
| `clean`                 |         | `cargo clean` and remove result links.                                                                                  | light | OK      |
| `clean-aux`             |         | Remove the auxiliary target subtrees (coverage, cross, doc) but keep debug/release.                                     | light | OK      |
| `cargo-provenance`      | `*args` | Read-only report of the cargo target dir's provenance (toolchain, sccache, dep-info).                                   | light | OK      |
| `test-cargo-provenance` |         | Hermetic fixture test for that report (Python and Bash).                                                                | light | OK      |

## Dev instance and running thegn

`live`/`devel` manage the DEV instance (`~/.thegn-devel`); the `start*` family runs throwaway isolated instances. `update` is new (see the quick start).

| Recipe                | Params                         | Purpose                                                                                                                                | Heavy                                   | Status |
| --------------------- | ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------- | ------ |
| `live`                | `[level size_mb files *flags]` | Build when inputs changed, install to `~/.thegn-devel/bin/thegn`, start the dev instance (`--here`, `--restart-daemon`, `--prebuilt`). | Mac (only when source changed)          | OK     |
| `live-plan`           | `[level size_mb files *flags]` | What `live` would do; changes nothing.                                                                                                 | light                                   | OK     |
| `update`              | `[target] [*args]`             | Update stable or dev from a verified release or a source build; no target prints status. Dispatches to `scripts/update_instance.py`.   | light (source targets build on the Mac) | OK     |
| `devel`               |                                | Dev instance with the DEBUG build (`target/debug/thegn`) via `scripts/devel-launch.sh`.                                                | Mac                                     | DUP    |
| `devel-env`           |                                | Print the dev instance environment as `export` lines.                                                                                  | light                                   | OK     |
| `devel-desktop`       |                                | Print a `.desktop` entry for the dev instance.                                                                                         | light                                   | OK     |
| `run`                 | `*args`                        | Build, then run a subcommand against the debug build (`just run list`).                                                                | Mac                                     | OK     |
| `start`               | `[name=dev]`                   | Build and run the host in an isolated `~/.thegn-<name>` state root.                                                                    | Mac                                     | DUP    |
| `attach`              |                                | Alias for `start`.                                                                                                                     | Mac                                     | DUP    |
| `start-dev`           | `[name=dev]`                   | Same as `start` with `THEGN_CHANNEL=dev`.                                                                                              | Mac                                     | DUP    |
| `start-mq`            | `[name gate]`                  | Same as `start` with the merge queue switched on via `--set`.                                                                          | Mac                                     | DUP    |
| `start-term`          | `[name backend]`               | Debug+profiling build in a dedicated ghostty window with every log channel on.                                                         | Mac                                     | OK     |
| `start-term-release`  | `[name backend]`               | Same rig with a release binary and maxed logging (daily-driver launcher).                                                              | Mac                                     | OK     |
| `dev-tui`             | `[name=dev]`                   | `cargo watch` that relaunches `start-term` on save.                                                                                    | Mac                                     | OK     |
| `dev`                 | `[what=shell]`                 | `nix develop` (or `dev tui` for `dev-tui`).                                                                                            | light                                   | OK     |
| `install`             | `*bindir`                      | Standalone (non-Nix) install via `install.sh`: builds release and installs `tg`, `tg-tui`, `thegn`.                                    | Mac                                     | OK     |
| `install-clean`       |                                | Fresh randomly named sandbox instance per run (`~/.thegn-clean-*`).                                                                    | Mac                                     | DUP    |
| `install-clean-sweep` |                                | Remove every `~/.thegn-clean-*` sandbox.                                                                                               | light                                   | OK     |
| `_apply-backend`      | `backend`                      | Private: write a sandbox-backend overlay for this repo used by `start-term*`.                                                          | light                                   | OK     |

## Tests

`test` is the unit gate shared by CI and the pre-push hook. The many `test-*` recipes are hermetic Python/Bash checks and are all dependencies of `test`.

| Recipe                     | Params       | Purpose                                                                                                                     | Heavy        | Status  |
| -------------------------- | ------------ | --------------------------------------------------------------------------------------------------------------------------- | ------------ | ------- |
| `test`                     |              | All hermetic helper tests, then `cargo nextest run --workspace` and the contract-test discovery pin.                        | Mac          | OK      |
| `test-doc`                 |              | `cargo test --doc --workspace` (CI only).                                                                                   | Mac          | OK      |
| `smoke`                    |              | Hermetic end-to-end scripts against the debug binary.                                                                       | Mac          | OK      |
| `smoke-pkg`                |              | Same smoke against the Nix-built package.                                                                                   | Mac          | OK      |
| `term-check`               |              | Terminal-capability matrix under degraded `TERM`/`LANG`.                                                                    | Mac          | OK      |
| `coverage`                 |              | Core line coverage gate at 95%, lcov to target/coverage.                                                                    | Mac          | OK      |
| `coverage-html`            |              | Coverage as an HTML report.                                                                                                 | Mac          | OK      |
| `e2e`                      |              | Every muse spec against a live binary in a PTY, hermetic env, ~8 min. Opt-in in CI.                                         | Mac          | OK      |
| `e2e-spec`                 | `spec`       | One muse spec under test/muse/specs/.                                                                                       | Mac          | OK      |
| `e2e-update`               |              | Re-record muse snapshot baselines.                                                                                          | Mac          | OK      |
| `e2e-glitch`               |              | Specs 18 to 28 (glitch-hunt subset).                                                                                        | Mac          | OK      |
| `sandbox-e2e`              |              | Podman-backed sandbox tests (`cargo test -p thegn-core -- sandbox`); skips without podman.                                  | Mac          | OK      |
| `sandbox-e2e-db`           |              | Tier 1 container-event DB audit test; runs in CI.                                                                           | Mac          | OK      |
| `sandbox-e2e-full`         |              | Tier 2 podman suite (six named integration tests); skips without podman.                                                    | Mac          | OK      |
| `sandbox-userns-e2e`       |              | User-namespace confinement scripts `test/userns-e2e.d/*.sh` (nightly).                                                      | light        | OK      |
| `security-e2e`             |              | THE-307 bwrap security gate (requires Linux, bwrap, userns).                                                                | Mac          | OK      |
| `plugin-conformance`       |              | `thegn plugin preview` and app-tab PTY conformance (needs the debug build; not in `ci`).                                    | light        | OK      |
| `test-windows-wine`        | `*args`      | Cross-compile tests for windows-gnu and run them under Wine.                                                                | Mac          | OK      |
| `test-windows-vm`          | `*args`      | Run the CI `windows` job's groups in the local Windows VM.                                                                  | Mac          | OK      |
| `windows-vm-setup`         |              | Create the local Windows 11 VM (needs `THEGN_WINVM_ACCEPT_LICENSES=1`).                                                     | light        | OK      |
| `windows-vm-up`            |              | Boot the VM headless and wait for ssh.                                                                                      | light        | OK      |
| `windows-vm-down`          |              | Clean VM shutdown.                                                                                                          | light        | OK      |
| `windows-vm-status`        |              | VM status.                                                                                                                  | light        | OK      |
| `windows-vm-destroy`       |              | Delete the VM and its state dir.                                                                                            | light        | OK      |
| `test-remote`              | `host *args` | Ship this worktree to an ssh host and run the gate there (`scripts/test-remote.sh`). The in-repo way to run tests on a Mac. | remote (Mac) | OK      |
| `test-remote-selftest`     |              | Hermetic check of the remote runner with an ssh shim.                                                                       | light        | OK      |
| `macos-ondevice`           | `outdir`     | Opt-in macOS OCI runtime capture on the Apple-silicon Mac.                                                                  | Mac only     | OK      |
| `macos-ondevice-selftest`  |              | Hermetic checks for the on-device evidence runner.                                                                          | light        | OK      |
| `verify-kani`              |              | Kani proofs for `termcaps` colour math. Documented as not compiling (libsqlite3-sys `cfg_select!`).                         | Mac          | UNCLEAR |
| `test-dev-live`            |              | Tests for `scripts/dev_live.py`.                                                                                            | light        | OK      |
| `test-update-instance`     |              | Tests for `scripts/update_instance.py` (no network).                                                                        | light        | OK      |
| `test-devel-launch`        |              | Tests for `scripts/devel-launch.sh`.                                                                                        | light        | OK      |
| `test-heredoc-guard`       |              | Tests for the PreToolUse heredoc guard.                                                                                     | light        | OK      |
| `test-build-metadata`      |              | Git-only build invalidation fixtures.                                                                                       | light        | OK      |
| `test-the429`              |              | Read-only checkout-hook regression.                                                                                         | light        | OK      |
| `test-action-pins`         |              | GitHub Actions must be pinned to commit SHAs; also the act guard self-test.                                                 | light        | OK      |
| `test-apply-backend`       |              | Backend overlay preservation regression.                                                                                    | light        | OK      |
| `test-nix-metadata`        |              | Nix package identity and licence drift.                                                                                     | light        | OK      |
| `test-nextest-setup`       |              | nextest isolate-state setup script tolerates /tmp churn.                                                                    | light        | OK      |
| `test-packaging`           |              | `packaging/release.py` admission and archive-limit tests.                                                                   | light        | OK      |
| `test-third-party-notices` |              | Notices generator test.                                                                                                     | light        | OK      |
| `test-changelog-lint`      |              | Self-test of the changelog heading lint.                                                                                    | light        | OK      |
| `test-net-trace`           |              | Net-trace summariser test with canned strace text.                                                                          | light        | OK      |
| `test-sprites-exec-repro`  |              | Sprites exec reproduction test.                                                                                             | light        | OK      |
| `test-remote-parity-tui`   |              | Remote-parity TUI probe in a throwaway HOME.                                                                                | light        | OK      |

## Gates and lint

| Recipe                | Params                              | Purpose                                                                                                                                                | Heavy                           | Status |
| --------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------- | ------ |
| `ci`                  |                                     | The full gate: lint, deps-audit, build, cross, features, msrv, test, doctests, docs, openspec, coverage, smoke, sandbox-e2e-db, term-check, nix-build. | Mac                             | OK     |
| `ci-local`            |                                     | `ci` plus the muse e2e suite.                                                                                                                          | Mac                             | OK     |
| `lint`                |                                     | Ratchets, perf-selftest, treefmt, clippy, shellcheck, yamllint, taplo, then `delivery-check`.                                                          | Mac                             | OK     |
| `ratchets`            |                                     | Source-only architecture ratchets and grep guards; no compile, seconds. Safe for merge-queue gates.                                                    | light                           | OK     |
| `ratchets-all`        |                                     | `ratchets` plus every Rust-side ratchet test (compiles).                                                                                               | Mac                             | DUP    |
| `contract-ratchets`   |                                     | Plugin API, control schema and surface-gap ratchets, with owner messages.                                                                              | Mac                             | DUP    |
| `ratchet-update`      |                                     | Regenerate every shrink-only ratchet allowlist.                                                                                                        | Mac                             | OK     |
| `help-ratchet-update` |                                     | Regenerate the help-page ratchet allowlists.                                                                                                           | Mac                             | OK     |
| `deps-audit`          |                                     | cargo-deny and cargo-machete.                                                                                                                          | light (needs network first run) | OK     |
| `fmt`                 |                                     | `nix fmt` (treefmt) in place.                                                                                                                          | light                           | OK     |
| `fmt-check`           |                                     | `nix fmt -- --ci`; `lint` already runs `treefmt --ci`.                                                                                                 | light                           | DUP    |
| `changelog-lint`      |                                     | CHANGELOG heading lint.                                                                                                                                | light                           | OK     |
| `delivery-check`      |                                     | Offline OpenSpec and delivery-state validation (date-dependent).                                                                                       | light                           | OK     |
| `integration-check`   | `crate filter first_tip second_tip` | Verify two lane tips are in HEAD, then clippy and nextest a crate.                                                                                     | Mac                             | OK     |
| `doctor`              |                                     | Report missing dev tools; non-zero if any are missing.                                                                                                 | light                           | OK     |
| `heal-git`            |                                     | Strip a leaked `core.worktree` from `.git/config`.                                                                                                     | light                           | OK     |
| `act`                 | `*ARGS`                             | Run the CI workflow locally with `act`.                                                                                                                | Mac (container)                 | OK     |
| `act-list`            |                                     | List the jobs `act` would run.                                                                                                                         | light                           | OK     |
| `act-job`             | `name`                              | Run one CI job locally.                                                                                                                                | Mac (container)                 | OK     |
| `act-clean`           |                                     | Remove leftover `act` containers.                                                                                                                      | light                           | OK     |
| `_act-check`          |                                     | Private: `act` credential and clean-HEAD guard.                                                                                                        | light                           | OK     |

## Performance

All A/B or machine-dependent; none are in CI. They refuse a stale or debug binary via `_perf-guard`.

| Recipe              | Params  | Purpose                                                         | Heavy | Status |
| ------------------- | ------- | --------------------------------------------------------------- | ----- | ------ |
| `bench`             |         | Startup benchmarks with hyperfine (builds release).             | Mac   | OK     |
| `bench-idle`        |         | Idle CPU benchmark over a worktree fixture.                     | Mac   | OK     |
| `bench-idle-record` |         | Record the idle reading as this machine's baseline.             | Mac   | OK     |
| `bench-steady`      |         | Steady-workload CPU benchmark.                                  | Mac   | OK     |
| `bench-soak`        | `*args` | 40-worktree resource soak (children, fds, threads, read rate).  | Mac   | OK     |
| `bench-soak-daemon` | `*args` | Soak with the pane daemon running.                              | Mac   | OK     |
| `bench-micro`       | `*args` | Criterion micro-benchmarks across the workspace.                | Mac   | OK     |
| `bench-micro-svc`   | `*args` | Git hot-path benches (`thegn-svc --bench git_hot`).             | Mac   | OK     |
| `perf`              |         | Umbrella: `bench`, `bench-idle`, `bench-micro`.                 | Mac   | OK     |
| `perf-flood`        | `*args` | Switch/input latency under an output flood.                     | Mac   | OK     |
| `perf-t3`           | `*args` | Workspace-switch latency.                                       | Mac   | OK     |
| `perf-selftest`     |         | Perf harness internals check; part of `lint`.                   | light | OK     |
| `_perf-guard`       |         | Private: refuse stale or debug binaries.                        | light | OK     |
| `profile`           | `*args` | Launch under the sampling profiler.                             | Mac   | OK     |
| `net-trace`         | `*args` | strace the first frame and list every connect() target (Linux). | Mac   | OK     |

## Release and packaging

The actual release is driven by RELEASING.md and the personal `release-alpha.py`; these recipes are the local rehearsals.

| Recipe                      | Params            | Purpose                                                         | Heavy              | Status  |
| --------------------------- | ----------------- | --------------------------------------------------------------- | ------------------ | ------- |
| `release-artifacts`         | `tag`             | Build the host archive and `.sha256` for this machine's target. | Mac                | OK      |
| `release-verify`            | `tag [host_only]` | Validate that archive, check its interpreter, run it.           | light              | OK      |
| `release-package-dry-run`   | `tag`             | Render package-manager inputs from synthetic archives.          | light              | OK      |
| `release-package-validate`  | `tag assets`      | Validate and render a dir of real archives.                     | light              | OK      |
| `rehearse-packages`         | `[deb rpm ...]`   | Rehearse deb/rpm in Debian, Ubuntu and Fedora containers.       | light (containers) | OK      |
| `third-party-notices`       |                   | Regenerate THIRD_PARTY_NOTICES.md.                              | light              | OK      |
| `third-party-notices-check` |                   | Fail when the notices are stale.                                | light              | OK      |
| `macos-app`                 | `[bin dest]`      | Generate the macOS `thegn.app` launcher.                        | light              | OK      |
| `icons`                     |                   | Re-render the owl icons from `owl.rs`.                          | light              | OK      |
| `image-build`               |                   | Build the base sandbox image and load it into podman.           | Mac                | OK      |
| `image-publish`             | `registry [tag]`  | Push the sandbox image for this arch to a registry.             | Mac                | OK      |
| `fly-image-publish`         | `registry [tag]`  | Build and push the Fly boot image.                              | Mac                | UNCLEAR |

## Live remote providers (real spend)

Plan-only unless explicitly armed.

| Recipe                 | Params  | Purpose                                           | Heavy | Status |
| ---------------------- | ------- | ------------------------------------------------- | ----- | ------ |
| `test-sprite`          |         | Live Sprites API integration tests (`#[ignore]`). | spend | DUP    |
| `sprites-live`         | `*args` | Live Sprites run (`test/sprites-live.sh`).        | spend | DUP    |
| `sprites-live-recycle` |         | Live sprite-recycle verification.                 | spend | DUP    |
| `remote-parity`        | `*args` | Provider-neutral remote parity run.               | spend | OK     |
| `remote-daily`         | `*args` | Daily headless agent run: pull, land, reap.       | spend | OK     |
| `vercel-live`          | `*args` | Live Vercel Sandbox run.                          | spend | OK     |

## Specs, delivery and agent tooling

| Recipe                 | Params      | Purpose                                                      | Heavy     | Status |
| ---------------------- | ----------- | ------------------------------------------------------------ | --------- | ------ |
| `openspec`             | `*args`     | Passthrough to the pinned openspec CLI.                      | light     | OK     |
| `openspec-setup`       |             | Regenerate the `/opsx` commands and skills under `.claude/`. | light     | OK     |
| `openspec-validate`    |             | Strict validation of every spec and change.                  | light     | OK     |
| `delivery-report`      | `*args`     | Portfolio totals from the index and task files.              | light     | OK     |
| `agent-context`        | `issue`     | Show issue-specific project memory offline.                  | light     | OK     |
| `graph-sync`           |             | Sync the per-worktree codegraph index.                       | light     | OK     |
| `graph-check`          |             | Read-only codegraph freshness check.                         | light     | OK     |
| `graph-find`           | `+query`    | Sync then `codegraph explore`.                               | light     | OK     |
| `memory-check`         | `*args`     | Agent memory check.                                          | light     | OK     |
| `agent-metrics`        | `csv`       | Agent metrics report from a CSV.                             | light     | OK     |
| `pipeline-tools`       | `state_dir` | Install lint tools into a pipeline worker's XDG state.       | Mac (nix) | OK     |
| `pipeline-tools-check` | `state_dir` | Show what a worker's login shell resolves for lint tools.    | light     | OK     |
| `demo-script`          | `*args`     | Opt-in scripted demo (muse and a release binary).            | Mac       | OK     |
| `demo-script-check`    |             | Hermetic validation of the demo steps.                       | light     | OK     |
| `fonts`                |             | List installed Nerd Font families.                           | light     | OK     |
| `font`                 | `name`      | Switch the bundled alacritty profile's font.                 | light     | OK     |
| `default`              |             | `just --list`.                                               | light     | OK     |

## Flagged

Nothing was deleted. Evidence for every non-OK row is below; each path or
symbol was checked in the tree.

### STALE

- **Stable-install prose.** `docs/help/two-instances.md` ("stable is updated through Nix (see the README)"), the `live` recipe comment ("The stable instance is installed by Nix/home-manager") and the `REMOVED_FLAGS` refusal in `scripts/dev_live.py` say Nix/home-manager owns stable. On this machine stable is the plain binary at `~/.local/lib/thegn/thegn` and no Nix package installs it. `just update stable` is now the supported path; those three texts need a follow-up edit.

### DUPLICATE or overlapping

- `attach` is an alias of `start`. `start`, `start-dev` and `start-mq` are three copies of one shell body that differ only by `THEGN_CHANNEL=dev` or the `--set merge_queue.*` flags; `install-clean` is the same launch with a random state dir. `start-dev` is the only one named in the docs (`two-instances.md`); `attach`, `start-mq` and `install-clean` are referenced by no doc, script or workflow.
- `devel` (debug build) and `live` (release build) both run the dev instance through `devel-launch.sh`; `host` and `run` overlap `start`.
- `fmt-check` (`nix fmt -- --ci`) duplicates the `treefmt --ci` step inside `lint`.
- `ratchets-all` runs `ratchets` plus the Rust ratchet tests that `test` already runs; `contract-ratchets` repeats three tests that `test` also selects (kept deliberately as a labelled preflight).
- `test-sprite`, `sprites-live` and `sprites-live-recycle` are three entry points to live Sprites runs; `test-sprite` is referenced by nothing.
- `sandbox-e2e` (`cargo test -p thegn-core -- sandbox`) overlaps `sandbox-e2e-full` (six named test binaries); neither is referenced outside the justfile.
- `release-profiling` / `build-profiling` and `start-term` / `start-term-release` / `dev-tui` are variants of the same rig.

### UNCLEAR

- **`release`** only runs `cargo build --workspace --release`; the name reads as the release process (`release-artifacts`, `release-verify`).
- **`verify-kani`**: its own comment says it does not compile (Kani's pinned nightly rejects `libsqlite3-sys`'s `cfg_select!`).
- **`fly-image-publish`**, `image-build`, `image-publish`, `icons`: unreferenced by docs or CI; the Fly devbox was retired 2026-11-20 but the Fly provider image may still be wanted.
- **Not wired into `test` or `ci`** (by design or by omission): `test-remote-selftest`, `macos-ondevice-selftest`, `plugin-conformance`, `third-party-notices-check`.

### Checked and fine

Every script, test file, spec, crate and recipe name referenced by a recipe body exists (`scripts/`, `test/`, `packaging/`, `nix/`, `config/`, `-p <crate>`, `--test <name>`, `--bench git_hot`, nested `just <recipe>` calls, the `cov_ignore` source list, the e2e spec range 18-28). Paths that look missing in a naive scan (`test/hostkey-`, `test/muse/specs/1`) are glob fragments inside comments and patterns.
