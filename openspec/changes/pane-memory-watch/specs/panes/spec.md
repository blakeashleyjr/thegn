# Panes

## ADDED Requirements

### Requirement: A pane warns near its memory cap and restarts after an OOM kill

For each local pane running in its own cgroup scope, the host SHALL sample the
scope's memory usage, cap, swap usage and event counters off the event loop. It
SHALL show a warning with the usage numbers when usage reaches 85% of the cap or
the `high` event counter rises, once per crossing, re-arming after usage falls
below 75%, and SHALL show a notice when the `oom_kill` counter increments. When
a pane's shell exits non-cleanly within 180 seconds after an `oom_kill`, the
host SHALL restart the pane in its layout slot and worktree, relaunching a
recognised agent with its continue command, at most once per ten minutes per
pane slot. It MUST NOT restart a pane that was closed by the user or whose
command exited cleanly, and MUST NOT type into a pane to deliver a warning.

#### Scenario: Usage crosses the threshold

- **WHEN** a pane's usage rises from 70% to 87% of its cap
- **THEN** one warning shows the usage, cap and swap, and no further warning
  shows until usage has fallen below 75% and risen again

#### Scenario: A pane is lost to an OOM kill

- **WHEN** a pane's `oom_kill` counter increments and its shell then exits with
  a failure
- **THEN** the pane is restarted in the same worktree and layout slot, and a
  recognised agent is relaunched with its continue command

#### Scenario: A second OOM exit within ten minutes

- **WHEN** a restarted pane is lost to another OOM kill less than ten minutes
  after the restart
- **THEN** it is not restarted automatically

#### Scenario: The command exits normally

- **WHEN** a pane's command exits with status zero, or the user closes the pane
- **THEN** it is not restarted, even if an `oom_kill` was recorded shortly before
