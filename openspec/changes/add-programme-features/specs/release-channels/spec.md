# release-channels Specification

## ADDED Requirements

### Requirement: Programme features are experimental and not yet implemented

thegn SHALL define nine experimental features for the sandbox and remote programme, with the ids
`sandbox_backends`, `linux_confine`, `egress_proxy`, `seatbelt`, `win_isolation`, `plugin_backends`,
`agent_presets`, `private_copy` and `escalation`. Each MUST be experimental, so the `stable` channel SHALL NOT
allow it, and none SHALL make any configuration key take effect until the work item that owns the feature
implements it. `thegn doctor` SHALL list each of the nine in both its human-readable and its `--json` output,
and SHALL describe each as experimental and not yet implemented until its owning item replaces that text. The
gate for private-copy workspaces MUST be named `private_copy`; the name `private_clone` MUST NOT be used.

#### Scenario: The nine ids are experimental

- **WHEN** the feature list is read for any of the nine ids
- **THEN** the feature is experimental and is not allowed in the `stable` channel

#### Scenario: Doctor lists the programme features

- **WHEN** the user runs `thegn doctor` or `thegn doctor --json` in any channel
- **THEN** all nine ids appear among the gated features, each described as experimental and not yet implemented

#### Scenario: Stable clamp output is unchanged

- **WHEN** a stable binary loads a config that predates the nine features
- **THEN** the clamped configuration is identical to what it was before they were added
