# Control Plane

## ADDED Requirements

### Requirement: Daemon failures are surfaced, not silent

Daemon degradation SHALL be visible without `THEGN_LOG`. The daemon status
chip MUST have an error state driven by heartbeat staleness — a crashed or
wedged daemon renders as an error, not as an unremarkable stale value — and
activating the chip SHALL show the on-demand probe's error detail,
distinguishing an unreachable socket from an alive-but-stale daemon. When a
daemon-backed spawn or reattach fails and the pane falls back to an
in-process PTY, the compositor MUST raise a notification naming the
degradation and its cause in addition to the existing log line. A daemon
panic SHALL produce a crash report like any thegn process (its stdio is
nulled, so the report is the only evidence), and the next attach or probe
SHALL surface that the daemon crashed, naming the report path. This
composes with the persistence chip: error state is additive on the same
chrome element and renders via the existing Full damage path with no new
wake source.

#### Scenario: Crashed daemon shows an error state

- **WHEN** the daemon's heartbeat goes stale beyond the staleness threshold
- **THEN** the status chip renders its error state, and activating it shows
  the probe's error detail

#### Scenario: Silent fallback becomes a visible degradation

- **WHEN** a daemon-backed pane spawn fails and the pane starts in-process
  instead
- **THEN** a notification states that the pane is running in-process because
  the daemon was unavailable, with the failure cause

#### Scenario: Daemon crash leaves evidence with logging off

- **WHEN** the daemon panics while `THEGN_LOG` is unset
- **THEN** a crash report with `proc=daemon` exists, and the next attach or
  chip probe surfaces the crash and names the report path
