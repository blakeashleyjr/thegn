## Why

A terminal created with an explicit `podman-rootless` pick, on a machine with no podman machine
running, spawned a bare host shell **and still reported itself as rootless podman**. The chain
resolved to `Backend::None`, `enter_argv` emitted a plain `sh -lc 'cd … && exec $SHELL'`, and the
label was copied from the _request_ rather than the result. A containment label that can disagree
with reality is worse than no label: it tells someone their agent is sandboxed while it runs on the
host with no kernel boundary. Nothing in the specs said the fallback must be reported truthfully —
only that it must happen.

The same machinery has a second gap: a runtime that is _installed but not running_ (stopped
`dockerd`, no `podman machine`, colima down) is one command away from working, but the resolver
silently treats it as absent and degrades to the host.

## What Changes

- **Containment is reported from the argv, not the request.** A new `thegn_core::sandbox_truth`
  module derives the backend from the command that is about to be executed and reconciles it
  against what was asked for, yielding the label, a `degraded` flag, and the warning to surface.
- **Both launch paths use it**: `panes.rs::terminal_launch_spec` (terminals — the reported bug) and
  `agent.rs::compose_spec` (worktrees/agents), the latter scoped to local placements, since a
  remote runtime sits behind a transport whose argv shape cannot be read locally.
- **A degraded terminal now falls through to a plainly labelled host shell** carrying the reason,
  instead of a container-labelled one.
- **The gate**: `every_backend_round_trips` renders the real `enter_argv` for every `Backend` and
  asserts the derived label matches, over a list that is exhaustive by construction — a new backend
  variant fails to compile the test rather than silently escaping the check. Companion tests pin
  the dangerous direction (an argument or path named `docker` must never promote a host shell into
  a claimed container).
- **Still to do in this change**: the `terminals` row records the _request_ at wizard-submit time
  and feeds the tab chip, so the chip can still misreport after a restart — intent and observed
  containment need separate columns. And a dormant runtime should offer **start / host anyway /
  cancel** rather than degrading silently.

## Capabilities

### New Capabilities

<!-- None: this constrains existing sandbox behavior rather than adding a capability. -->

### Modified Capabilities

- `sandbox`: `Graceful backend selection` gains the requirement that a fallback is reported
  truthfully; new requirements cover argv-derived containment labels, the exhaustiveness gate,
  separating recorded intent from observed containment, and offering to start a dormant runtime.

## Impact

- **Roadmap**: `tasks.md` AB.350 (sandbox per worktree), AB.362 (default-on with `--no-sandbox`
  escape), AO.490 (doctor/diagnostics) — this is the truthfulness property those all assume.
- **Code (done)**: `crates/thegn-core/src/sandbox_truth.rs` + `sandbox_truth_tests.rs` (new),
  `crates/thegn-core/src/lib.rs`, `crates/thegn-host/src/panes.rs`,
  `crates/thegn-host/src/agent.rs`.
- **Code (pending)**: `crates/thegn-host/src/handlers/terminal.rs` (writes the request),
  `crates/thegn-host/src/hydrate_terminal.rs` (renders it), `db_workspace.rs` + `db_migrate.rs`
  (column split + `user_version` bump), and the dormant-runtime prompt over the existing
  `sandbox_support::BackendState::NotRunning` + `remedy_for` detection.
- **Behavior change users will notice**: a sandbox pick that cannot be honoured now says `host` and
  warns, where it previously showed the requested runtime. This is the point of the change, but it
  will look like a regression to anyone who trusted the old label.
- **No new dependencies.**
