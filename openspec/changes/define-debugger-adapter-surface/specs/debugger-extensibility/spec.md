# Debugger Extensibility

## ADDED Requirements

### Requirement: Debugger extension layers have explicit support states

thegn SHALL classify launch adapters, DAP transport, and plugin-contributed
debugger UI independently as supported, reserved, deferred, or rejected. Public
CLI help, config examples, API/plugin schemas, and help MUST NOT imply support
for a layer until its runtime and acceptance tests are delivered.

#### Scenario: DAP is not implemented

- **WHEN** a user inspects debugger and plugin documentation before DAP lands
- **THEN** only the supported BugStalker CLI handoff is claimed and no reserved
  enum/config vocabulary is described as usable DAP support

### Requirement: Adopted launch adapters are safe and deterministic

IF generic launch adapters are adopted, each adapter SHALL use trusted config,
validated argv templates rather than shell strings, explicit platform and
run/attach capability metadata, deterministic selection, and detection-only
doctor reporting. The built-in BugStalker adapter MUST remain the compatibility
default and produce the pre-registry argv when no adapter is selected.

#### Scenario: Worktree declares an executable adapter

- **WHEN** an untrusted worktree-local config supplies an adapter command
- **THEN** it is ignored or rejected under the central config-trust policy and
  cannot become executable merely by opening the worktree

### Requirement: Adopted DAP and debugger UI use a bounded provider contract

IF DAP or a debugger UI provider is adopted, the contract SHALL version and
bound messages, queues, payloads, timeouts, cancellation, lifecycle, and crash
recovery; separate process-launch from display permissions; keep protocol work
off the UI loop; and render only host-sanitized tokenized content under host
placement and input ownership.

#### Scenario: Provider stalls or crashes

- **WHEN** a debugger provider exceeds its deadline or exits unexpectedly
- **THEN** the host remains responsive, tears down or degrades the surface
  deterministically, and grants no additional process authority on restart
