# THE-29 architect revision 1

Status: REVISE

The following findings block approval. Each is concrete and must be addressed
in a follow-up chunk, with focused tests added in the same change.

## 1. Forked PTY geometry and handoff history do not inherit the source

- `crates/thegn-host/src/daemon/service.rs:788-789` always spawns a fork at
  `24x80`, although the design and OpenSpec require the source's current
  geometry.
- `crates/thegn-host/src/daemon/session.rs:420-426` limits the optional fork
  handoff to `TOMBSTONE_HISTORY_LINES` (500), while the design requires the
  same `SNAPSHOT_HISTORY_LINES` bound used for a warm snapshot (2,000).

Fix expected: capture the live source rows/columns without disturbing the
source, pass those dimensions to the child PTY, and use one named history-bound
policy shared with the snapshot path. Add tests that resize a source and verify
the child dimensions, and that the handoff uses the documented bound.

## 2. An explicit source harness can be replaced by the agent's provider

- `crates/thegn-host/src/daemon/service.rs:717-756` validates and plans the
  requested `ForkPlan::Harness`, but then discards `plan.command` and resolves
  the command again from `AgentLaunch.agent`.
- `crates/thegn-host/src/daemon/agent_open.rs:70-76,238-253` selects the
  provider from that agent name. Thus a request such as
  `harness=claude&agent=<agent configured for codex>` can pass core validation
  as Claude and spawn Codex with the Claude native id, or otherwise silently
  fork the wrong provider.

Fix expected: keep the selected source harness authoritative. Either reject an
agent whose configured provider does not match the explicit harness, or extend
the launch-resolution seam so it preserves the selected harness command while
still applying the configured agent's current credentials/sandbox composition.
Do not regenerate a potentially different vendor command in generic daemon
code. Add a mismatch test and a successful configured-agent/native-harness test.

## 3. Fork orchestration caused avoidable god-file growth

- `crates/thegn-host/src/daemon/service.rs:616-877` contains the complete fork
  source resolution, history/file lifecycle, agent re-resolution, PTY spawn,
  registration, adoption, and cache write path (over 260 lines), while
  `daemon/fork.rs` only contains small helpers.

Fix expected: extract the fork orchestration and/or a shared session-spawn
helper into the focused daemon fork module, leaving `DaemonService::fork` as a
thin service boundary. Preserve the existing service's async/`spawn_blocking`
boundaries and best-effort annotations; do not add another parallel spawn path.

## 4. The active OpenSpec change is not synchronized with the implementation

- `openspec/changes/add-session-fork/proposal.md:64-76` still says MCP is not
  exposed in v1, while the implementation advertises and dispatches
  `sessions.fork` on MCP.
- `openspec/changes/add-session-fork/design.md` retains the same MCP exclusion
  in its security section and does not describe the native recorded-harness
  source that this branch implements.
- `openspec/changes/add-session-fork/tasks.md:5-35` leaves all delivered tasks
  unchecked, and its `ForkSpec` description omits the implemented harness,
  agent, adopt, and source-discrimination semantics.

Fix expected: update the active proposal/design/tasks and control-plane spec to
the binding THE-29 design (including MCP/plugin/catalog projection, native
harness capability behavior, and the final wire fields), mark only completed
tasks complete, and run `openspec validate --all --strict`. The change must not
claim MCP exclusion while `SurfaceSet::ALL` remains implemented.

## 5. Required daemon behavior is not integration-tested

- `crates/thegn-host/src/daemon/fork.rs:77-99` tests only path/cleanup helpers;
  the recorded chunk result lists only two `daemon::fork` tests.
- There is no daemon-level test covering the actual `ControlApi::fork` path for
  source liveness, new id/pid, dead-session refusal, identity environment,
  resized geometry, handoff lifecycle/permissions, or adopt intent.

Fix expected: add hermetic daemon tests using temporary state and a disposable
PTY command. Assert the source remains live and unchanged, the child has a
different id/pid and inherited dimensions, validation failures spawn nothing,
handoff files are owner-only and cleaned on exit, and adoption records the
requested placement. Keep all state isolated from the normal XDG state dir.
