# Lead

## ADDED Requirements

### Requirement: The Lead is set up from inside the app

When the Lead jump key is pressed and `[lead]` is absent, or the Lead is not logged in, or the
daemon's Lead listener is not up, thegn SHALL open a guided setup instead of opening the Lead. The
setup SHALL check bubblewrap and pasta (both required; a missing pasta shows install hints and never
a host-network option), preview the `[lead]` and `[daemon] enabled` lines before writing them with
the config writer, confirm before restarting the daemon, run the agent and MCP logins by running
`thegn lead login` in a thegn pane, and offer a reset that removes `[lead]`. Only claude SHALL be offered as the Lead's agent.

#### Scenario: Unconfigured jump

- **WHEN** the jump key is pressed with no `[lead]`
- **THEN** the setup menu opens and no Lead terminal is created

#### Scenario: Resume

- **WHEN** the setup is interrupted and the jump key is pressed again
- **THEN** the step shown is derived from the config file and host state, not remembered state

#### Scenario: Stable channel

- **WHEN** the running channel is stable
- **THEN** the setup describes the Lead as a preview, writes `[lead] preview = true` with the rest
  of the config, and continues to the daemon restart with no relaunch step

### Requirement: Stable keeps an explicitly set-up Lead

On the stable channel the config clamp SHALL keep `[lead]` only when `[lead] preview = true`, and
SHALL still remove a `[lead]` without it. Panes and the daemon stop the flow starts SHALL NOT force
`THEGN_CHANNEL=dev`, and the doctor SHALL accept a stable daemon for a preview Lead.

#### Scenario: Marker present

- **WHEN** a stable build loads a config with `[lead] preview = true`
- **THEN** `[lead]` stays configured and the doctor channel check passes

#### Scenario: Marker absent

- **WHEN** a stable build loads a config with `[lead]` and no `preview`
- **THEN** `[lead]` is clamped off as before

### Requirement: Entry points, inline checks and a counted restart

The sidebar SHALL show a "Lead — set up" row, on both channels, while no `[lead]` is configured, and
the welcome tour's agents chapter SHALL offer a key that leaves the tour and opens the same flow.
The flow SHALL run the Lead checks in a background task and show the report in its last step, and
its restart confirm SHALL state how many live daemon sessions would end.

#### Scenario: Setup row

- **WHEN** no `[lead]` is configured and the sidebar row is activated
- **THEN** the setup flow opens and no Lead terminal is created

#### Scenario: Counted restart

- **WHEN** the user chooses to restart the daemon with 3 live sessions
- **THEN** the confirm says 3 live daemon sessions would end before anything is stopped

### Requirement: Lead startup failures remain readable and recoverable

The Lead startup screen SHALL wrap the complete failure detail at narrow widths. A cold-switch
panel load SHALL replace its skeleton with panel content or an explicit error and retry instruction;
it SHALL NOT leave a skeleton after its worker exits without a panel result.
Jumping to an existing Lead whose listener is unavailable SHALL open the setup flow, which offers
the documented daemon restart path. Confirming SHALL run `thegn daemon stop` off the event loop,
wait for the stop to finish, and reopen the Lead so its next attach starts a daemon with the listener.
When the current channel or host prerequisites prevent launch, the sidebar Lead entry SHALL show a
one-line reason and route to setup rather than launching a failing Lead.

#### Scenario: Narrow startup error

- **WHEN** the Lead startup screen has a failure message wider than its subline at a narrow terminal
- **THEN** the complete message, including its recovery instructions, is wrapped below the failed step

#### Scenario: Panel hydration fails

- **WHEN** a cold-switch panel worker cannot load the selected path or database
- **THEN** the panel replaces its skeleton with an error and tells the user how to retry

#### Scenario: Existing Lead lost its listener

- **WHEN** the user jumps to an existing Lead whose daemon has no Lead listener
- **THEN** the setup flow offers to restart the daemon and retry the Lead

#### Scenario: Restart retries Lead

- **WHEN** the user confirms restart from the setup flow
- **THEN** the UI waits for `thegn daemon stop` to finish and reopens the Lead

#### Scenario: Lead launch is unavailable

- **WHEN** the current channel or host lacks a Lead requirement
- **THEN** the sidebar entry shows the reason and opens setup instead of launching Lead
