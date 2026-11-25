# Lead

## ADDED Requirements

### Requirement: The Lead is set up from inside the app

When the Lead jump key is pressed and `[lead]` is absent, or the Lead is not logged in, or the
daemon's Lead listener is not up, thegn SHALL open a guided setup instead of opening the Lead. The
setup SHALL check bubblewrap and pasta (offering the host network when pasta is missing), preview
the `[lead]` and `[daemon] enabled` lines before writing them with the config writer, confirm before
restarting the daemon, run the agent and MCP logins by running `thegn lead login` in a thegn pane,
and offer a reset that removes `[lead]`. Only claude SHALL be offered as the Lead's agent.

#### Scenario: Unconfigured jump

- **WHEN** the jump key is pressed with no `[lead]`
- **THEN** the setup menu opens and no Lead terminal is created

#### Scenario: Resume

- **WHEN** the setup is interrupted and the jump key is pressed again
- **THEN** the step shown is derived from the config file and host state, not remembered state

#### Scenario: Stable channel

- **WHEN** the running channel is stable
- **THEN** the setup explains the dev channel and, after the config write, tells the user to
  relaunch with `THEGN_CHANNEL=dev`
