## MODIFIED Requirements

### Requirement: Graceful backend selection

The sandbox SHALL select an isolation backend by preference order podman -> docker -> bwrap -> none, MUST fall back to the next when a runtime is unavailable, and MUST fall back to `none` (run on the host) rather than failing to launch when no backend exists. Every fallback MUST be reported truthfully: the containment reported for a launch MUST describe what that launch actually entered, never what was requested.

#### Scenario: Preferred runtime missing

- **WHEN** podman is not installed but docker is
- **THEN** the worktree process launches under docker

#### Scenario: No runtime available

- **WHEN** none of podman, docker, or bwrap is available
- **THEN** the process runs with backend `none` on the host and the worktree is
  still usable

#### Scenario: Fallback is reported, not hidden

- **WHEN** an explicit backend pick cannot be honoured and the launch degrades to the host
- **THEN** the launch is labelled `host`, is flagged as degraded, and carries a warning naming the
  backend that was unavailable

## ADDED Requirements

### Requirement: Containment is derived from the executed argv

The containment backend reported for a pane SHALL be derived from the argv that pane executes, not
from the configured, persisted, or user-selected backend. A resolver that returns a container
backend while composing a bare host shell MUST produce a `host` label.

Derivation MUST consider only words in command position, so that an argument that merely contains a
runtime's name — a worktree path, a git remote, an image reference — can never promote a host shell
into a claimed container. Where a launch is wrapped by a transport that hands off to a runtime
(`sudo -n podman`, `kubectl exec … -- podman …`, an ssh remote command), the runtime MUST still be
recognised.

#### Scenario: A requested runtime is not running

- **WHEN** a terminal is created with an explicit `podman-rootless` pick on a host with no podman
  machine running
- **THEN** the pane is labelled `host`
- **AND** the user is warned that the pane is running on the host with no kernel boundary

#### Scenario: A path named after a runtime

- **WHEN** a host shell is launched in a worktree whose path contains a segment named `docker`
- **THEN** the pane is labelled `host`

#### Scenario: Rootless and rootful podman stay distinct

- **WHEN** containment is derived for a rootless podman argv and a rootful (`sudo -n podman`) argv
- **THEN** the two produce different labels, matching their respective backends

#### Scenario: Containment invisible to the argv

- **WHEN** the backend contains through the spawn syscall rather than the argv (the native-Windows
  job-object and AppContainer backends)
- **THEN** the requested backend is reported, because argv inspection can neither confirm nor deny
  it, and this exception is documented at the derivation site

### Requirement: The containment label is gated against every backend

The mapping from a launch argv back to its backend SHALL be verified against the real argv builder
for every supported backend, over a backend list that is exhaustive by construction, so that adding
a backend or changing how one is spelled fails the build rather than silently reopening a false
containment claim.

#### Scenario: A backend is added

- **WHEN** a new `Backend` variant is introduced without extending the gate
- **THEN** the test suite fails to compile

#### Scenario: A backend's argv changes shape

- **WHEN** a backend's rendered argv no longer maps back to that backend
- **THEN** the round-trip test fails

### Requirement: Recorded intent is distinct from observed containment

A per-worktree or per-terminal sandbox pick SHALL be persisted as a deliberate override so it
survives restarts and can be re-resolved later. That recorded intent MUST NOT be used as the value
displayed as the pane's containment; display MUST come from what the launch actually entered, so a
pick that could not be honoured on one run is neither forgotten nor shown as fact.

#### Scenario: A pick that could not be honoured, across a restart

- **WHEN** a terminal was created with a container pick that degraded to the host, and thegn is
  restarted
- **THEN** the chip reports the containment the pane actually has
- **AND** the original pick is still used when re-resolving, so it takes effect once the runtime is
  running

### Requirement: A dormant runtime is offered, not silently skipped

When a launch would degrade because a runtime is installed but not running, thegn SHALL distinguish
that state from "not installed" and offer the user a choice — start the runtime, continue on the
host, or cancel — rather than degrading silently. Starting MUST run off the event loop with visible
progress, MUST invalidate the runtime probe cache, and MUST re-resolve afterwards. The choice SHALL
be configurable for users who want one answer every time.

#### Scenario: A stopped runtime at launch

- **WHEN** a sandboxed pane is launched while a container runtime is installed but its service is
  not answering
- **THEN** the user is shown the runtime, the reason, and the command that would start it
- **AND** may start it, continue on the host, or cancel

#### Scenario: Starting from the prompt

- **WHEN** the user chooses to start the dormant runtime
- **THEN** the start command runs off the event loop with progress shown
- **AND** the backend is re-probed rather than answered from the cached "absent" result

#### Scenario: Continuing on the host

- **WHEN** the user chooses to continue on the host
- **THEN** the pane launches labelled `host` and flagged as degraded
