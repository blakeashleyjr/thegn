# Debugger — audit delta

## ADDED Requirements

### Requirement: Doctor reports the supported debugger truthfully

`thegn doctor` SHALL report the built-in BugStalker tool's resolution tier,
resolved path when present, installed-versus-pinned state, and Linux x86-64
platform restriction. The probe MUST be detection-only and MUST NOT launch or
attach a debugger.

#### Scenario: Unsupported host explains the gate

- **WHEN** doctor runs outside Linux x86-64
- **THEN** its debugger result identifies the platform restriction rather than
  implying that setup can make the debugger runnable

#### Scenario: Managed version is inspected safely

- **WHEN** doctor evaluates an installed BugStalker binary
- **THEN** it reports its resolution and pin status without starting a debug
  session
