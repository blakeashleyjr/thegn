# THE-29 chunk 3 completion

Implemented daemon-owned session forking and the user-facing surfaces.

## Delivered

- Added live-session recipe retention, bounded history-tail handoff, owner-only
  scrollback files, identity environment propagation, lineage metadata, and
  best-effort handoff cleanup.
- Added fresh harness fork resolution through `AgentLaunch`, raw-session cap
  wrapping, child registration/events, credential-free fork records, and dead
  session errors pointing to `sessions.open`.
- Added `thegn session fork`, optional worktree creation/cwd remapping, JSON and
  lineage output, MCP `sessions.fork`, and the `fork-session` pane action with
  sibling/new-tab adoption through the existing graft path.
- Updated daemon/session/configuration/CLI help, action ratchets, and the
  isolated-state CLI smoke check.

## Verification

- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host daemon::fork` — 2 passed.
- `cargo nextest run -p thegn-host session_fork` — 3 passed.
- `cargo nextest run -p thegn-host adopt` — 18 passed.
- `cargo nextest run -p thegn-host help` — 76 passed.
- `bash -n test/smoke.sh`, `git diff --check`, and `cargo fmt --all -- --check` — passed.

## Unverified

- `cargo nextest run -p thegn-host daemon::service` ran 21/22 tests successfully;
  `ws_warm_attach_pipeline_over_a_real_socket` failed at socket setup with
  `Operation not permitted` in the restricted environment. The remaining
  service tests passed.
- The CLI smoke script was syntax-checked but not executed because no scoped
  debug binary was available without starting an additional build; no e2e or
  live-state invocation was run.

## Commits

- `742f2c53` — checkpoint daemon session fork
- Final docs/summary commit uses the required subject:
  `feat(the-29): daemon session fork CLI and UI`
