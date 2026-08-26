# Tasks

## 1. Enforcement matrix (pure core + doctor)

- [x] 1.1 New pure module `thegn-core/src/sandbox_matrix.rs`: `EnforcementRow`
      (fs / net / ceiling-strength / scoping / class) with `row(backend, os)`
      derived from `Capabilities::from_parts_on`, `backend_suitable_on`, the
      profile family, and a `CpuCap`-shaped ceiling rule — no second policy
      table. Exhaustive `match` over `Backend` × `HostOs` (compile-fails on a
      new variant, the containment-label gate pattern).
- [x] 1.2 Unit tests to the 95% core gate: every cell of the Linux, macOS, and
      Windows columns pinned per-OS from one machine (the `from_parts_on(os)`
      idiom), including: Linux host-toolchain ceiling degrades hard→soft with
      delegation absent; macOS local OCI class is guest-kernel; unverified
      backends under-promise.
- [x] 1.3 Fix the `HostProcess` escape note: drop the Landlock/Seatbelt claim
      (thegn applies neither); reword to "no kernel boundary; resource/lifetime
      scoping only where wrapped".
- [x] 1.4 `thegn doctor`: render the host's matrix column beside the existing
      `sandbox_support` rows; ceiling cells reuse the probed `CpuCap` label;
      unverified rows carry the `mark-unverified-backends` caveat.
- [x] 1.5 `docs/help/` sandbox page: document the matrix and what each cell
      means (no new action ids; prose only, keep the help ratchets green).

## 2. Isolation floor

- [x] 2.1 Config: `[sandbox] isolation_floor` (empty | `shared-kernel` |
      `userspace-kernel` | `guest-kernel` via `config_enum!`; `host-process`
      rejected by validation as always-met) and `on_floor_miss`
      (`degrade` default | `fail`). Documented `config/config.toml.example`
      entries.
- [x] 2.2 Pure comparison in core: class ordering, `ProviderManaged` outside
      the order (bypass + report), comparison over the resolved spec's honest
      class after runtime degrade. Unit tests: floor met by runtime raise,
      degraded-runtime compares as what it became, macOS VM-mediated OCI
      satisfies guest-kernel, Windows jobobject satisfies nothing at or above
      shared-kernel.
- [x] 2.3 Launch path: apply the floor after backend/runtime resolution;
      `degrade` reuses the existing degraded-flag + warning + deduped
      notification path; `fail` aborts before any host spawn with the
      actionable error (floor, best available class, remedy). Compose with
      the dormant-runtime prompt (starting the runtime re-resolves the floor).
- [x] 2.4 Trust clamp: repo overlay may only raise the floor / harden the miss
      policy; denial surfaced through the existing clamp reporting +
      `thegn config explain sandbox.isolation_floor`.
- [x] 2.5 Ssh placement: compare over what the remote probe proves;
      `Unreachable` counts as a miss (design open question 2 resolved in
      implementation).

## 3. Windows honesty

- [x] 3.1 Reclassify `Backend::WinJobObject` to the host-process class in
      `capabilities::isolation_for`; keep `WinAppContainer` as the reserved
      container-class boundary. Update the containment-label gate and the
      pinned per-OS classification tests.
- [x] 3.2 Matrix Windows column: OCI-declined-by-policy and
      limits-deferred cells stated while true (reconcile wording with
      `add-windows-parity`).

## 4. Agent-workload floors

- [x] 4.1 Extend the agent-task configuration (coordinate with
      `add-agent-task-engine`, which owns the key shape) with the sandbox
      opt-in + `isolation_floor`; default posture unchanged (host + slice).
- [x] 4.2 `agent_run.rs`: wrap the task argv via `sandbox::enter_argv` when
      opted in; floor check per phase 2; keep `wrap_background_argv`
      fail-safe semantics for the resource wrap.
- [x] 4.3 Failure attribution: a fail-closed floor miss or sandbox setup
      failure surfaces as an infrastructure failure (queue entry held/retried),
      never a branch/agent failure. Unit-test the attribution split.
- [x] 4.4 Document the recommended agent posture in
      `config/config.toml.example` (floor `shared-kernel`, `fail` on Linux,
      `degrade` elsewhere).

## 5. smolvm verification (gated; no code before 5.1 passes)

- [ ] 5.1 Verify smolvm's real CLI surface on live installs (Linux + macOS):
      create/exec/stop/remove verbs, `--volume <wt>:<wt>` path-preserving
      directory bind, exit codes and stderr shapes. Record findings in this
      change folder.
- [ ] 5.2 If verified: liveness argv + enter/teardown wiring (OCI-family arms,
      or a dedicated family if the verbs diverge), flip the matrix row to
      guest-kernel, drop the `verified()` caveat for `Backend::Smol`.
- [ ] 5.3 If the bind is not path-preserving: keep the backend reserved and
      record why (the bind-at-real-path invariant is non-negotiable).

## 6. Finish

- [ ] 6.1 Run `just ci` once (includes openspec validate) as the pre-PR gate.
