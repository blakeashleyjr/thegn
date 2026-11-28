# Update notice Specification

## Purpose

Describe how stable releases notify users of newer public releases without
making startup or rendering depend on network availability.

## Requirements

### Requirement: Check and present newer stable releases

Stable builds SHALL check the latest public GitHub release at most once per day
in the background, cache the result in the state directory, and fail silently
when the network is unavailable. The check SHALL be disabled by
`[update] check = false`; dev-channel builds SHALL skip it. When a release is
newer than the running version, the statusbar SHALL show a dismissible notice
and `thegn doctor` SHALL report the version and an install-appropriate upgrade
command.

#### Scenario: Newer release is available

- **WHEN** a stable build receives a newer release tag
- **THEN** the statusbar shows an update notice and doctor reports its version
  and upgrade command

#### Scenario: Network is unavailable

- **WHEN** the release endpoint cannot be reached
- **THEN** startup and rendering continue without an error notice

#### Scenario: User disables checks or runs a dev build

- **WHEN** `[update] check = false` or the resolved channel is dev
- **THEN** no release request is made
