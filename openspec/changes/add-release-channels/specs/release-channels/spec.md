# Release Channels

## ADDED Requirements

### Requirement: The binary runs in a resolved release channel

thegn SHALL operate in exactly one release channel — `stable` or `dev` —
resolved once at startup. The channel MUST be resolved from `THEGN_CHANNEL`
(values `stable`/`release` or `dev`/`experimental`) when set, otherwise from the
compiled-in default (the host `dev` Cargo feature ⇒ `dev`, else `stable`). An
unset or unrecognised `THEGN_CHANNEL` MUST fall back to the compiled default
rather than error. The same resolution MUST apply to the interactive compositor
and to non-interactive CLI verbs so a user sees one consistent view.

#### Scenario: Default stable build

- **WHEN** a binary built without the `dev` feature starts with `THEGN_CHANNEL`
  unset
- **THEN** the resolved channel is `stable`

#### Scenario: Env override wins over the compiled default

- **WHEN** any binary starts with `THEGN_CHANNEL=dev`
- **THEN** the resolved channel is `dev`, and `THEGN_CHANNEL=stable` forces
  `stable` even on a binary built with the `dev` feature

### Requirement: Experimental subsystems are disabled in the stable channel

In the `stable` channel thegn SHALL neutralise every experimental subsystem at
config load so an experimental key left in a user's config is inert rather than
half-active. The gated set is: remote worktrees (`[sandbox.remote]`), execution
providers (`[host.*]`), the LLM proxy (`[llm_proxy]`), the Observe dashboards
(`[observe]`), the placement engine (`[placement]`), and the non-GitHub issue
trackers (`[issues]`: Linear/Jira/Kaneo). GitHub PR/issue viewing MUST remain
available in both channels, and the `[[agents]]` launcher list (which includes
the plain-shell entry) MUST NOT be cleared. In the `dev` channel the clamp MUST
be a no-op.

#### Scenario: Stable clamps experimental toggles

- **WHEN** a stable binary loads a config with `[llm_proxy] enabled = true`,
  `[observe] enabled = true`, `[placement] enabled = true`, and
  `[sandbox.remote] host = "box"`
- **THEN** each of those master toggles reads back as off (`host` empty), and a
  one-line status note reports which features were disabled

#### Scenario: Dev honours experimental toggles

- **WHEN** a dev binary (or `THEGN_CHANNEL=dev`) loads that same config
- **THEN** every toggle is honoured unchanged

#### Scenario: GitHub trackers survive the tracker clamp

- **WHEN** a stable binary loads `[issues] providers = ["linear", "github", "kaneo"]`
- **THEN** the resolved providers are `["github"]`

### Requirement: The channel and its allowances are inspectable

`thegn doctor` SHALL report the resolved channel and, for every gated feature,
whether the current channel allows it — in both the human-readable output and
the `--json` output. This is the authoritative answer to "why is this feature
disabled?".

#### Scenario: doctor reports the channel

- **WHEN** the user runs `thegn doctor` in the stable channel
- **THEN** the output names the channel `stable` and lists each gated feature as
  disabled (dev-only), with a pointer to the dev build

### Requirement: Experimental CLI verbs are refused in the stable channel

thegn SHALL refuse experimental non-interactive verbs (`proxy`, `agent`, `host`,
`placement`, `kaneo`) in the stable channel with a clear error naming the
feature and how to enable it (the dev build or `THEGN_CHANNEL=dev`), rather than
silently doing nothing. In the `dev` channel those verbs MUST run normally.

#### Scenario: A gated verb in the stable build

- **WHEN** the user runs `thegn proxy status` in the stable channel
- **THEN** the command exits with an error explaining it is a dev-channel
  feature and how to enable it, without contacting the proxy
